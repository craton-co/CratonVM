# Concurrent GC Maturation: G1 validation plus selectable ZGC-real backend

Status: implementation progress + remaining validation plan. Target branch: `design-concurrent-gc`.
Author note: this doc is grounded in a read of `gc/src/{g1,g1_concurrent,zgc,zgc_concurrent,concurrent_mark,satb,vm_heap}.rs`,
`vm/src/config.rs`, `vm/src/vm/vm_init.rs`, `vm/src/runtime/interpreter.rs`, and
an internal code-review pass that surfaced the SATB drain contract gap (finding #18,
addressed in Step 3 / section 2.6 below) and a `gc-collectors` module assessment.

### Implementation progress

- **Step 1 (CLI flag wiring) - DONE** (branch `feat/g1-cli-flag`, updated by `codex/zgc-feature-concurrent-maturation-20260709`). `-XX:+UseG1GC`
  now selects the G1 backend; `-XX:-UseG1GC` reverts to Generational; `-XX:+UseZGC`
  now selects the memory-backed `ZgcRealHeap` backend. Other `-XX:+Use*GC`
  selectors (Serial/Parallel/Shenandoah/Epsilon) warn and fall back to Generational
  (lenient-with-warning, section 3.1). Generational remains the default. Implementation:
  `parse_gc_algorithm()` in `vm/src/config.rs`; `-XX:+Use<name>GC` -> `--XX:UseGc <name>`
  rewrite + `gc_selector` clap field + config-apply in `vm-cli/src/main.rs`; `GcAlgorithm`
  -> `GcBackend` mapping and `PrintFlagsFinal` strings in `vm/src/vm/vm_init.rs`.
- **Step 2 (doc truth-up) - DONE** (branch `feat/g1-cli-flag`, updated by `codex/zgc-feature-concurrent-maturation-20260709`). Reconciled the
  docs-governance gap now that G1 and ZGC-real are selectable: Generational remains the
  default safety net, G1 is opt-in via `-XX:+UseG1GC`, and ZGC-real is opt-in via
  `-XX:+UseZGC`. The simulation-only colored-pointer ZGC scaffolding remains explicitly
  non-production and separate from the selectable real heap.
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
  - **GAP C — Panama/FFM upcall targets never rooted (MEDIUM, both collectors)
    — FIXED.** The libffi upcall trampoline dispatched to a Java target reachable
    only through a leaked `UpcallUserdata.target` (an `ObjectRef`) that nothing
    scanned or remapped — a moving-GC UAF on the next upcall once the target was
    collected or relocated. Fix: make `UpcallUserdata.target` an `AtomicUsize`
    (remappable in place; the trampoline loads it per dispatch), hold the leaked
    userdata pointer in `UPCALL_REGISTRY`, and export
    `panama::{gc_scan_upcall_target_roots, gc_update_upcall_target_refs}` wired into
    the VM root scan (`roots.rs`) and `update_all_roots` (`gc.rs`) — the same
    `gc_scan_*`/`gc_update_*` idiom xnio/selector/classloader use. Regression test
    `upcall_target_root_scan_and_remap_gap_c`; the existing
    `new18_upcall_libffi_closure_dispatches_to_java` exercises the atomic-load
    trampoline read. (Note: the *legacy* `ctx.register_upcall` slot-table copy is a
    separate, pre-existing un-rooted path — out of scope here.)
  - **GAP D — ec_watch table not remapped on multi-thread GC paths (LOW,
    diagnostic-only) — FIXED.** Added the missing `ec_watch::remap` after
    `update_all_roots` in the multi-threaded `maybe_gc_forced` and
    `force_gc_from_native` initiator paths (the single-threaded paths already had
    it). Diagnostic consistency only — no production UAF.
  - The audit also DOWNGRADED a `monitor_on_exit` over-claim: it is reachable via
    local-0 / class-mirror in the normal case, so it is in `pointer_map`; the
    residual (a method overwriting local 0) is pre-existing and not G1-specific.
  - Remaining for Step 5: the GAP A fix shipped a per-slot remap correction; a
    fuller moving-GC stress test (multi-thread precise-JIT frame + FFM upcall
    relocated under `-XX:+UseG1GC`) asserting no staleness is still worthwhile.
- **Step 6 (JNI-critical region pinning under G1) — DONE** (branch
  `feat/g1-jni-critical-pin`). The §3.2.5 gap, refined: the merged force-copy fix
  (#24) closes the *data-movement* half — `GetPrimitiveArrayCritical` hands native
  code a detached **copy**, never a heap pointer, so relocating the source is
  harmless to the native reads. But `ReleasePrimitiveArrayCritical`'s **copy-back**
  re-resolves the array's *Get-time* handle (`jobject_to_obj(copy.array)`, a raw
  local-ref address); the keep-alive pin is "keep-alive ONLY, not no-relocation",
  so under G1 a young/mixed evacuation mid-section moves the array and that handle
  goes stale → the copy-back silently drops (CSet region freed → `is_heap_addr`
  None) or writes back into a recycled object (corruption). G1 makes this reachable
  (unconditional young/mixed evacuation); the generational young-from is swapped
  not freed and critical sections are short, so it stayed latent there — matching
  the doc's "G1 makes this exploitable".
  - **Fix:** drive the existing (previously test-only) `G1Region` pin machinery
    from the critical path. `jni_get_primitive_array_critical` calls
    `VmHeap::pin_critical_region(array)` → `G1Collector::pin_region_for_addr`
    (lock-free `lookup_region_for_addr` + refcounted `pin_region`), recording the
    pinned region index(es) in the `CriticalCopy`;
    `jni_release_primitive_array_critical` calls `unpin_critical_regions` on the
    **final** release (NOT `JNI_COMMIT` mode 1, whose section continues and must
    stay pinned). G1's only object-moving paths — `young_collection` /
    `mixed_collection` — exclude pinned regions from the collection set, and there
    is no full-GC compaction path, so a pinned array cannot move while checked out
    → the copy-back resolves the same object.
  - **Refcounted** (`G1Region.pin_count`, mirrored by the existing `pinned` bool
    the CSet filters read): overlapping critical sections on arrays in the same
    region, and nested checkouts of one array, pin/unpin independently — a single
    `bool` would let an inner Release clear a pin an outer section still holds.
    No-op (empty pin set) on the generational collector.
  - **GetStringCritical** needs no pinning: it delegates to `GetStringChars`
    (copy) and strings are immutable, so `Release` frees the copy with **no
    copy-back** — there is no stale-handle write to guard.
  - **Scope note:** the *non-critical* `Get<Type>ArrayElements` force-copy path
    has the same latent copy-back staleness under G1, but region pinning is the
    wrong fix there (the JNI spec permits long-lived element copies; pinning would
    hold regions arbitrarily long) — it needs a remappable copy-back handle.
    Pre-existing, tracked as a separate follow-up.
  - **Validation:** `cratonvm-gc` g1 tests green — new
    `pin_region_for_addr_keeps_jni_critical_array_in_place` (a young GC leaves a
    pinned array's address unchanged + data intact) and `region_pin_refcount_balances`;
    full g1 suite + `cratonvm-cli` build green; FieldStress checksum unchanged
    under `-XX:+UseG1GC`.
- **Step 7 (pause-target CSet sizing) — DONE** (branch `feat/g1-pause-cset-sizing`,
  merged to dev `ff370857`). `max_gc_pause_ms` (200) previously did **not** bound
  the mixed collection set — old regions entered the CSet only under the
  percentage cap (`old_cset_region_threshold_percent`, 10%), so a mixed pause
  could grow unbounded with old-gen occupancy (§3.3). Added a **time-budget cap**
  on top of the percentage cap:
  - `G1Region::estimated_evac_cost_ns(ns_per_byte) = live_bytes × ns_per_byte`
    (copying live data dominates evacuation cost).
  - A **rolling** `evac_ns_per_byte` calibration (EMA, default 4 ns/byte ≈
    250 MB/s), refreshed from each *mixed* collection's actual
    `pause / bytes_copied` so the budget tracks real wall-clock copy throughput
    (the §3.3 "rolling per-region copy cost"). Calibrated from mixed GCs **only**
    — young collections would bias it high (fixed root-scan overhead amortized
    over few survivors).
  - Both the inline `mixed_collection` CSet build (production) and the
    `select_old_regions_for_mixed_gc` helper now stop adding old regions once
    their estimated copy time would exceed `max_gc_pause_ms`, always keeping ≥1
    region for progress; deferred regions are reclaimed in a later mixed cycle
    (`mixed_gc_remaining`). The percentage cap stays the hard upper bound; the
    budget only binds when one mixed GC would copy enough live old data to blow
    the target (a genuinely long pause), so at the 200ms default it is
    conservative and rarely binds.
  - The standalone `select_evacuation_candidates`/`crate::region` prototype
    already had this algorithm but on a *different* region type, unused by the
    real collector — Step 7 brings it to the production `G1Region` path.
  - **Tests:** `g1region_estimated_evac_cost_scales_with_live_and_rate`,
    `evac_cost_ema_calibrates_toward_observed`,
    `mixed_cset_old_selection_respects_pause_budget`; all 93 g1 unit tests +
    `cratonvm-cli` build green (also verified on the merged dev alongside the
    `af64d03d` JNI copy-back follow-up).
  - **Owed:** empirical pause-vs-target validation on a real workload (§5) still
    needs the pause-logging enhancement *and* a non-crashing G1 run — currently
    blocked by the gpu-bench-cpu SIGSEGV (Step 8 finding).
- **Step 8 (opt-in G1 gauntlet validation) — IN PROGRESS. First cut found a real
  G1 SIGSEGV; G1 is NOT yet gauntlet-ready** (branch `feat/g1-step8-correction`).
  - **METHODOLOGY CORRECTION (supersedes the earlier "no divergence" claim,
    commit `92d3349d`/merge `ea1e35f2`).** That first run used a pre-existing
    release binary built at **11:15** which *predated* the Step-1 `-XX:+UseG1GC`
    flag merge (`7c6fdb35`, **12:54**), so it silently fell back to **Generational**
    — every "CV-G1" column was Generational-vs-Generational, not a G1 validation.
    Caught via the `--verbose:gc` startup line printing the *Generational arm's*
    `[GC] Verbose GC logging enabled (generational collector)` message (G1 has no
    such message) plus the absence of G1-specific `[GC YoungOnly]` collection
    logs. **Lesson — always verify the collector is actually selected before
    claiming a G1 result** (guard: zero "(generational collector)" messages +
    G1-only `[GC YoungOnly]` lines under `RUST_LOG=cratonvm_gc=info --verbose:gc`).
    Re-validated on a binary where `-XX:+UseG1GC` genuinely selects G1 (verified).
  - **G1-SPECIFIC CRASH (the headline finding) — `gpu-bench-cpu` `CpuOnlyBench`
    SIGSEGVs under G1.** `rc=139`, **3/3 deterministic** under `-XX:+UseG1GC` at
    `--Xmx 512m`; **3/3 PASS** under Generational. A hard segfault (no stack — it
    crashes before the 120s watchdog), i.e. a genuine **moving-GC memory-safety
    bug** on a real workload — exactly the class Steps 1–6 targeted, and totally
    masked while the run was accidentally Generational. Root-cause is a focused
    GC-debugging follow-up (spawned task).
  - **Checksum parity on verified G1 (G1 == Generational == HotSpot)** — both
    low-GC and GC-stress: `fib44`=701408733, `sieve250k`=22044,
    `matrix800`=15359906451, and crucially the heavy-evacuation
    **`bintrees18`@8g=68332206** are byte-identical. So G1 itself does **not**
    lose / duplicate / corrupt objects under sustained young+mixed evacuation —
    the gpu-bench crash is a distinct memory-safety fault, not a tracing bug.
  - **G1 "heap inefficiency" was mostly a SILENT-CORRECTNESS bug — now ROOT-CAUSED
    and FIXED** (merge `8bb638d9`, fix `ffb60014`). The original read ("not a
    correctness bug, just region overhead — `bintrees18` needs ~8g on G1 vs 4g
    gen") was WRONG. Root cause: serial `alloc_in_type_locked` chose evacuation
    *destination* regions by type and reused a partially-filled Survivor (young GC)
    / selected Old (mixed GC) region that was **itself in the collection set**, so
    survivors were copied INTO a region Phase 5 then resets (frees) — **silent
    live-object loss on every young GC after the first** (the first has no Survivor
    regions yet, which masked it). So G1 only produced correct results at heaps big
    enough to NEVER collect; the moment it GC'd it corrupted (hence the giant
    `-Xmx`). Distinct from the JIT missed-root issue (A5, below): this reproduces
    with `--nojit` and in the interpreter. Repro `scratch/g1par/DeepTree.java`: a
    held 65535-node tree, `-XX:+UseG1GC --nojit -Xmx64m DeepTree 15 3000` returned
    `got=1` (lost 65534 nodes), now `got=65535`; `binarytrees16 --nojit` was
    silently wrong at every GC-triggering heap (14721206..14079350 vs 14985902),
    now 14985902 down to 48m. **Fix** = thread the CSet into `evacuate_object` →
    `alloc_in_type_locked` and skip CSet regions (the semi-space "never allocate
    into from-space" invariant gen and the Step-9 parallel TLAB path already
    honour). **Residual genuine footprint gap is now ~20%** (G1 completes
    `binarytrees16` at 48m vs gen 40m), not the ~2x the corruption implied. (NB the
    bogus first cut also "passed" `bintrees18`@4g because it was actually gen.)
  - **App-suite no-regression on real G1**: **h2-testall-fast** PASS==PASS.
    **hibernate-smoke** was RED on **both** collectors (non-GC: ByteBuddy
    `JavaDispatcher$DynamicClassLoader.proxy` `jsr/ret` verifier rejection) —
    **now FIXED** and GREEN by default on both collectors. Root cause: the
    HIGH-sec commit `ecfc3b30` made *every* `jsr/jsr_w/ret` method a hard
    `VerifyError`, but that class is **major 49 (Java 5)** where the opcodes are
    *legal* (JVMS §4.9.1 forbids them only at major ≥ 51) and HotSpot loads it.
    Fix (`classloading/src/verifier.rs`): version-gate the rejection — accept
    structurally-validated subroutines at major ≤ 50 (HotSpot's load decision),
    keep the hard `VerifyError` at major ≥ 51 (genuinely malformed; HotSpot
    rejects too). `CRATONVM_ALLOW_JSR_RET=1` is no longer needed for legal old
    classes; it remains an any-version override. Verified: `HIB_SMOKE_OK` rc=0
    under CratonVM **default flags** == HotSpot; 98/98 verifier unit tests green.
    FieldStress (Step 4) stays 4495525842000 under G1.
  - **Throughput**: `matrix800` G1/gen = **1.05** (CV-G1 5410ms vs CV-default
    5163ms; CV-G1 ≈ 2.6× HotSpot-G1 2069ms — the interpreter+baseline-JIT gap).
  - **Pause-time logging enhancement (§7 item 6) — DONE** (branch
    `feat/g1-pause-logging`). The old `[GC ...] pause=Nms` line was
    `as_millis()`-granular (sub-ms young pauses rounded to 0) and only surfaced
    via `RUST_LOG=cratonvm_gc=info`. Replaced with a **microsecond** record sink:
    every young/mixed evacuation path (serial + parallel) now funnels through a
    `record_collection(type, pause_us, stats)` helper that appends a
    `G1PauseRecord` to a bounded ring (`PAUSE_HISTORY_CAP = 64K`, oldest evicted +
    counted), and `pause_summary()` reduces it to per-type p50/p99/max via
    nearest-rank. Visible **without RUST_LOG**: `--verbose:gc` emits a parseable
    per-collection `[GC-STAT] type=… pause_us=… bytes_*=…` line straight to
    stderr, and at VM shutdown (`--verbose:gc` or `CRATONVM_GC_STATS=1`) a
    `[GC-SUMMARY] young|mixed count=… p50_us=… p99_us=… max_us=…` table is dumped
    (`g1.rs::{record_collection,log_gc_event,pause_summary,print_gc_summary}`,
    `vm_heap.rs::print_gc_summary`, `vm-cli/src/main.rs` shutdown hook). Unit
    tests `pause_history_records_each_collection`, `pause_percentiles_nearest_rank`,
    `pause_history_ring_is_bounded`; 734/734 gc tests green.
  - **Pause-time + throughput — OBTAINED** (verified G1, `cvg1pause.exe`;
    harness `scratch/g1par/g1pause-measure.sh`). All checksums byte-identical
    HotSpot==Gen==G1 throughout.
    - **Small-live-set churn (`SteadyChurn 20000000 --nojit`):** CV-G1 young
      pauses are in HotSpot's regime — @32m p50/p99 = **2.7 / 3.7 ms** (61 GCs),
      @24m **1.9 / 4.7 ms** (84 GCs), @16m **1.4 / 2.9 ms** (135 GCs). Pauses
      scale sensibly with eden size. (HotSpot scalar-replaces this bench's
      non-escaping garbage → 0 GCs, so no per-pause comparison there.)
    - **Large-live-set evacuation (`binarytrees 16 @64m --nojit`):** real
      allocation on both. HotSpot-G1 = 9 GCs, **p50/p99 = 1.8 / 4.7 ms**;
      CV-G1 serial = 27 GCs, **p50/p99 = 39 / 58 ms** (~10–20×). This is the
      single-threaded serial evacuator copying a large live set under the
      interpreter — exactly the case parallel evacuation targets.
    - **Parallel evacuation cuts the large-live-set pause** (`PromoteMixed
      200000 20000000`, `CRATONVM_G1_PARALLEL_EVAC=1`, 4 workers): @48m young
      p50 **269 → 141 ms** (1.9×), @64m **148 → 72 ms** (2.0×); mixed p50 also
      drops (@96m **42 → 27 ms**), all checksums = HotSpot `200039989800000`.
      **But the parallel evacuator is not yet trustworthy** — it has an OPEN rare
      young-collection race (Step-9 "Parallel evacuator" bullet, defect 2) that
      corrupts `SteadyChurn @16m`, so these are a *potential* pause win, not a
      shippable one until that race is fixed.
    - **Throughput (`SteadyChurn 20000000 @32m --nojit`):** CV-G1 35.4 s vs
      CV-Gen 33.0 s = **1.07×** (within margin). Both ≈230× HotSpot's 0.15 s
      (escape-analysis + full JIT) — the interpreter/baseline-JIT gap §5 already
      acknowledges ("same regime, not beating HotSpot").
    - **Honest read:** the pause-target *direction* is honoured (pauses bounded
      and heap-scaled), CV-G1's pause is within HotSpot's single-digit-ms regime
      for small live sets, and ~10–20× for large live sets where the *serial*
      evacuator dominates. Parallel evacuation *would* close much of that
      (1.5–2× measured) but is blocked by an OPEN correctness race (Step-9 bullet,
      defect 2); a persistent worker pool (vs the per-GC `thread::scope` spawn) is
      a further throughput lever once the race is fixed.
  - Harness (untracked scratch in the worktree): `g1-step8-revalidate.sh` (now
    GUARDS collector selection up front), `g1-step8-benchparity.sh`,
    `g1-step8-appsuites.sh`.
  - **Remaining for Step 8**: (1) ✅ DONE — `gpu-bench-cpu` G1 SIGSEGV fixed;
    (2) ✅ DONE — GC-stress checksum parity on G1 confirmed (now byte-identical
    even at GC-triggering heaps after the evacuate-into-CSet fix `ffb60014`);
    (3) ✅ ROOT-CAUSED + FIXED — the "heap-efficiency gap" was mostly the
    evacuate-into-CSet silent-corruption bug (above); residual genuine footprint
    overhead is ~20%; (4) ✅ DONE — pause-logging enhancement + p50/p99 +
    throughput obtained (above); **and JIT known-issue A5 is now FIXED on dev**
    (`77c98761` — unregistered compiled-`main` JIT frame → non-moving sweep +
    full-stack mark; G1+JIT no longer corrupts at GC-triggering heaps), so the
    G1+JIT multi-GC differential is unblocked; (5) the daemon boot/e2e comparison
    — still gated on 3 tracked **non-G1** upstream bugs that stop
    WildFly/ES/Kafka/Spring Boot reaching *ready* even on Generational (non-TTY
    stdout SEGV/hang, ARRAY-LEN-GUARD, GC-clinit; see `apps/TARGET_APPS.md`).
- **Step 9 (parallel evacuation) — FOUNDATION + FULL MULTI-THREADED EVACUATOR DONE**
  (foundation: branch `feat/g1-parallel-evac-foundation`; evacuator: branch
  `feat/g1-parallel-evac`). The gpu-bench-cpu G1 SIGSEGV that gated the §3.4
  "fast-after-correct" caveat is FIXED (dev `f8f357e1` + `85d16997`), so the
  multi-threaded evacuator now lands — still **default-off**, opt-in via
  `CRATONVM_G1_PARALLEL_EVAC=1`. The serial path is unchanged and remains the
  default; the public `young_collection`/`mixed_collection` dispatch to new
  `young_collection_parallel`/`mixed_collection_parallel` only when the flag is set.
  - **Foundation (behaviour-identical, checksum-neutral, already on dev):**
    `evacuate_object` returns `Option<(*mut u8, bool)>` where the bool is `fresh`
    (true iff this call performed the copy). The `scan_and_evacuate_refs` ref-scan
    sites gate their work_list push on `fresh` instead of a separate
    `pointer_map.contains_key(...)` pre-check, removing the explicit evacuation
    TOCTOU. `CRATONVM_G1_PARALLEL_EVAC` flag declared (read-once `OnceLock`,
    default off).
  - **The four-piece evacuator (now implemented, `gc/src/g1.rs`):**
    1. **Atomic forwarding** — the dedup/race winner is decided by a CAS on each
       from-space object's own `ObjectHeader::forwarding_ptr` (treated as an
       `AtomicUsize` via `addr_of_mut!`). G1's serial young/mixed never uses the
       from-space `forwarding_ptr` (it uses `pointer_map`) and every live object
       starts a collection with a null `forwarding_ptr`, so the field is free to
       repurpose as the per-object install slot. CAS winner copies (`fresh=true`);
       a loser abandons its speculatively-copied destination (unreferenced
       to-space garbage reclaimed next cycle) and adopts the winner's address.
       Only worker-vs-worker races exist (mutators parked at the STW safepoint).
    2. **Per-worker forward shards** — each worker records its winning `(old,new)`
       pairs into a thread-local `Vec`, merged into `GcResult.pointer_map` after
       the closure (consumed by the VM root remap, Phase-4 region remap, monitors
       and the mark worklist exactly as the serial map). No concurrent map needed.
    3. **Per-worker GC-TLAB allocation** — replaces the single big `regions.lock()`
       for allocation. Workers claim whole Free regions from a shared pool via a
       lock-free `fetch_add` index, then bump-allocate with a thread-local cursor
       (single owner per region ⇒ no intra-region atomics); the cursor is written
       back on retire.
    4. **Shared work queue** — `Mutex<Vec<usize>>` of gray to-space addresses with
       per-worker batches and an `outstanding` termination counter (children added
       before the parent is subtracted, so it never transiently hits 0 while work
       remains). The design's explicit "first cut" in place of work-stealing
       deques (`crossbeam-deque` is available transitively if profiling later
       shows the shared-lock contention matters).
  - **Safety model.** The `regions.lock()` guard is held for the whole collection
    (the concurrent marker, which takes the same lock per step, stays excluded).
    The driver derives the regions' raw base once (`as_mut_ptr`) and does NOT
    deref the guard again until after the `std::thread::scope` join; in between,
    CSet (from-space) regions are only read + atomically CAS'd, and each to-space
    region is `&mut`-accessed only by its unique claiming worker (`split_at_mut`
    -style disjointness). Roots + RSet sources are seeded serially by the driver
    (the bulk — the transitive closure — is the parallel part); the driver also
    participates as one worker.
  - **Validation.**
    - **Unit (the copy-path correctness proof):** 9 new g1 tests call the parallel
      methods directly (flag-independent) — basic, reference chain,
      unreachable-freed, pointer-map root remap, **wide fan-out (2000 distinct
      objects, no loss/dup, 8 workers)**, **diamond shared-children CAS dedup
      (200 parents × 50 shared children evacuated exactly once under 8 workers)**,
      promotion, mixed, and **serial-vs-1-worker-vs-8-worker byte-identical
      equivalence** (objects_copied + reachable-value multiset). 725/725 gc tests
      green (isolated/single-threaded; the `gen_heap` parallel-pollution failure
      is pre-existing and reproduces with these tests skipped). Stress-run 8× with
      zero flakiness.
    - **Real binary (`cvg1par.exe`, verified G1 — no "(generational collector)"):**
      a one-shot `g1: parallel evacuation ACTIVE (N workers)` log confirms the
      gated path is genuinely taken. Completing workloads are byte-identical
      across HotSpot / CV-serial / CV-parallel (`binarytrees18@8g`=68332206,
      `binarytrees16@4g`=14985902). Under actual GC (`SteadyChurn`@256m) the
      parallel evacuator engages (4 workers) and behaves **byte-for-byte
      identically to serial** (same `freed=`, same outcome).
    - **Diverse-workload differential under sustained `--nojit` GC** (now that the
      evacuate-into-CSet fix lets G1 collect correctly): object trees
      (`binarytrees16`@64m, 30 GCs), held-graph (`DeepTree`), primitive arrays
      (`IntArrChurn`), real `HashMap` (`HashChurn`), `String`/StringBuilder
      (`StrChurn`), and pointer churn (`SteadyChurn`) all give one checksum across
      HotSpot / gen / serial-G1 / parallel-G1 at GC-forcing heaps. No new
      correctness divergence found. Two NON-correctness items characterized:
      (1) **parallel evac spawns a `thread::scope` worker pool per GC**, so it is
      contention-sensitive and carries per-collection thread-spawn overhead
      (a persistent worker pool is the throughput follow-up — the design's noted
      "first cut"; this also made some `--verbose:gc`+`RUST_LOG` parallel runs hit
      the 120s watchdog under concurrent-session CPU load — a measurement artifact,
      not a hang: the same runs complete correctly in isolation);
      (2) ✅ **evacuation-failure under to-space exhaustion — FIXED** (merge
      `cdb62510`, fix `40ba24d9`). At a heap too small to fit the live set (where
      gen correctly OOMs), G1 used to silently DROP the objects it couldn't
      relocate (`evacuate_object` returned `None` → caller skipped → Phase 5 freed
      the still-referenced region → wrong result). Now both evacuators
      **self-forward** on alloc failure (identity forward `old→old`; the parallel
      path CASes the from-space `forwarding_ptr` to its own address) and the new
      `free_or_keep_cset` helper KEEPS any CSet region holding a self-forwarded
      object (Eden→Survivor) instead of freeing it — so nothing is lost, the heap
      stays full, and the triggering allocation fails into a clean catchable OOM.
      `GcChurn`@96m now raises `OutOfMemoryError` on serial-G1 AND parallel-G1
      (matching gen) instead of a wrong checksum; clean-heap parity unchanged
      (binarytrees16/DeepTree/GcChurn@256m); 727/727 gc tests + a zero-free-region
      regression. Distinct from both the CSet fix and JIT A5.
  - **Old→young remembered-set-completeness hole — FIXED** (merge `da4cbe7a`, fix
    `8069818f`; found during the mixed-GC validation pass). A `young→young` ref
    carries no rset entry (young is collected whole); when the holder ages /
    promotes to **Old** while the referent stays younger, the edge silently becomes
    Old→young — but it was created and maintained ONLY by GC-internal pointer
    rewrites (evacuation slot-fixups + the promotion copy), never a mutator write
    barrier, so the rset never learned of it and the next young GC dropped the
    still-live young referent (the V7b verifier reported 75k–104k dangling refs
    from Old holder regions). It is the general **aging/promotion** case of the
    JIT-pinned-straddle fix `5d761809`, but reproduces with `--nojit`/interpreter
    (no JIT roots) and is a pre-existing serial bug (the parallel evacuator only
    avoided it by faster timing). Fix: `update_references_in_regions` (the Phase-4
    pass that already walks every non-CSet⇒Old/Humongous region each collection)
    now also rebuilds the Old→young rset at no extra walk — every cross-region
    reference into an Eden/Survivor region is recorded via
    `RememberedSet::add_reference` so the next collection scans it as a source
    (shared serial+parallel; over-approximation only). Repro
    `scratch/g1par/MixedChurn.java` (humongous `live[]` of cross-referenced nodes),
    `-XX:+UseG1GC --nojit`: @160m serial was **3/3 WRONG** with V7b≈75k–104k; now
    **V7b=0 and serial is correct@{192,224,256}m or a clean OOM@160m — never
    wrong**; parallel@160m + clean-heap unchanged; 727 gc tests + regression
    `old_to_young_ref_via_gc_rewrite_not_dropped`. **Follow-ups (both now done):**
    the analogous **Old→Old / humongous→Old** GC-rewrite edge for MIXED GC is now
    also recorded by the same Phase-4 rebuild (extended to all collectable targets;
    bug C above, dev `3d067512`), and the IHOP/region/pause `-XX:` knobs are wired
    (`cfae0f03`) so a mixed cycle can be forced with a low IHOP — which is exactly
    how the marking/mixed-GC repair was driven and validated.
  - **Known-issue A5 (G1+JIT moving-GC corruption) — ✅ FIXED on dev `77c98761`.**
    With the JIT enabled, a long-lived local ref held across a hot loop was missed
    by GC root scanning, so G1's precise unconditional moving young collection
    freed the still-reachable graph (`copied=0`, whole heap freed) → corruption →
    OOM. Root cause was **not** register-residency (the earlier framing) but the
    compiled entry-point `main`'s **JIT frame being unregistered** — `Vm::invoke`
    pushes no `JitEntryGuard`, so `gc_quiescence` was blind to it and the moving
    young collector relocated its roots without being able to rewrite the raw
    stack slots. Fix: `scan_active_jit_frames` detects a JIT code address on the
    native stack when the entry chain is empty → full-stack mark + per-thread flag
    → non-moving sweep (pin) for that collection. So the G1+JIT multi-GC
    differential that this Step could not previously sustain is now unblocked.
    Residuals: Windows-only; the chain-non-empty case.
  - **Remaining:** ✅ the JIT-root blocker (A5) is fixed; a diverse soak drove many
    serial + parallel GCs end-to-end and is clean for SERIAL, but **surfaced an
    OPEN parallel-evacuator data race** (defect 2 in the parallel-evac bullet
    below) — the hard prerequisite for parallel-evac-default-on and thus Step 10.
    Then: a persistent worker pool (vs the per-GC `thread::scope` spawn) and the
    optional work-stealing-deque upgrade for throughput. Flipping the flag
    default-on (with a demonstrated large-heap throughput win) is part of Step 10.
- **CENTRAL GAP — G1 concurrent marking + mixed GC — ✅ FIXED (A+B+C), merged to
  dev `3d067512`** (was the root of the footprint gap; old-gen is now reclaimed
  by mixed GC). Driving the now-wired `-XX:` knobs to force a mixed cycle had
  revealed three distinct bugs, all now fixed:
  - **A — marking never STARTS:** `collect_garbage` prematurely flipped the phase
    to ConcurrentMark (activating SATB, no marker) → the VM's real
    `maybe_concurrent_gc` gate (`should_start && !is_marking_active`) was blocked.
    Fix: delete the premature flip; the VM layer drives the full cycle. (g1.rs)
  - **B — marking never COMPLETES (deadlock):** the marker worker parks (stays
    `is_running`) at a fixed point and only exits on `request_stop`, but
    `g1_concurrent_mark_finished` polled `!is_running` and `request_stop` is only
    issued after completion. Fix: the worker publishes a `quiesced` AtomicBool at
    its fixed point; `g1_concurrent_mark_finished` polls quiescence; `g1_mark_roots`
    clears it after seeding (premature-quiesce race). (g1_concurrent.rs, vm_heap.rs)
  - **C — mixed evacuation DROPPED live objects** (`copied=3`, V7b≈72k dangling):
    a referent reachable only through a GC-internal **Old→old / humongous→old**
    edge was freed. An `A.a=B` edge created while both ends are young records only
    the (later-recycled) young source region; once A/B age or promote to Old the
    edge is maintained ONLY by GC-internal pointer rewrites, never a mutator
    barrier, so the referent's Old region never learns of the source and a mixed
    GC that selects it never scans the source. **Fix** (the Old→old/humongous→old
    generalization of the Old→young fix `8069818f`): the Phase-4 rset rebuild
    (`update_references_in_regions`) now records cross-region edges into every
    *collectable* (Eden/Survivor/Old) target via the renamed
    `collect_outgoing_cross_region_edges` + `is_collectable_region_type` — no
    extra walk, dedup'd; the humongous source is walked via the existing
    contiguous-arena `cursor=size` path (the earlier "`scan_source_region` can't
    walk a humongous source" read was wrong post-arena-fix; the real miss was the
    rset edge). Repro `scratch/g1par/PromoteMixed.java` with
    `-XX:InitiatingHeapOccupancyPercent=5 -XX:MaxGCPauseMillis=1 --nojit`:
    serial mixed GC is now **byte-identical to HotSpot (200039989800000) with
    V7b=0 dangling** across 8–55 mixed cycles at {48,64,96}m, humongous AND
    non-humongous holder; binarytrees14–17 byte-identical; `cratonvm-gc` 730/730
    (new regression `humongous_to_old_ref_via_gc_rewrite_survives_mixed_gc`,
    verified to fail without the Old target).
  - **Parallel evacuator — TWO defects; one fixed, one OPEN (the Step-9/10
    blocker).**
    - **Defect 1 — self-forward (evacuation-failure) UAF — ✅ FIXED on dev
      `7e97111e`.** The parallel evacuator CAS-installs each forward into the
      from-space object's persistent `forwarding_ptr`; a *self-forwarded* object
      (to-space exhausted → `old→old`) lives in a region `free_or_keep_cset`
      KEEPS, so its `forwarding_ptr` survived into the next cycle, short-circuited
      re-evacuation, and the region was then freed → UAF. Fix: `parallel_evacuate`
      clears every self-forward's `forwarding_ptr` after merging the shards (the
      `key == value` loop). This was the "dangling refs on humongous-ref-array"
      symptom that originally gated mixed→serial.
    - **Defect 2 — TWO stacked bugs; DOMINANT 🟡 FIXED (dev `f1afdcf9`), residual
      race 🔴 OPEN.** Repro `CRATONVM_G1_PARALLEL_EVAC=1 … --nojit -Xmx16m
      SteadyChurn 2000000` threw `java/lang/Object`; always correct serially.
      **(1) DOMINANT — persistent-`forwarding_ptr` root-remap flaw, FIXED.** The
      parallel evacuator dedups via the PERSISTENT `ObjectHeader.forwarding_ptr`
      (serial uses the per-cycle `pointer_map`); a fast-path hit returned a
      forward — possibly a stale prior-cycle one — WITHOUT recording it in
      `pointer_map`, so the VM's `update_all_roots` could not remap a root that
      resolved through the header → the frame local stayed stuck on the from-space
      object and dangled on region reuse. Invisible to V7b (heap-only). Fix =
      `evacuate` fast path records `(old, existing)` + `parallel_evacuate` clears
      `forwarding_ptr` for ALL keys at cycle end (combining the two fixes that
      fail alone — clear = 28/30 bad, record = 8/30). `SteadyChurn @16m` ~12.5%→0
      (`CRATONVM_G1_DBG_HEADERS` LOST=0); `binarytrees16`/`PromoteMixed`
      serial+parallel == HotSpot; 734/734 + regression. **(2) RESIDUAL — a
      separate transient worker-vs-worker concurrency race**, ~2/40, verifier
      reports nothing and the rate rises under its timing perturbation (the
      original `task_58d60f7a` race). STILL OPEN; needs concurrency tooling.
    - **Consequence:** because defect 2 lives in the shared `parallel_evacuate`
      closure that both young and mixed parallel paths drive, **mixed GC stays on
      the SERIAL evacuator** (the earlier plan to un-gate it was reverted — its
      premise that the evacuator was fully correct after `7e97111e` is false). The
      parallel young path stays opt-in/experimental under the flag (it was already
      racy; this characterizes the race, it does not introduce it). Where parallel
      runs DID complete they were checksum-correct (`PromoteMixed` 48/64/96m,
      `binarytrees`, `DeepTree`, `IntArrChurn`), and parallel pauses are ~1.5–2×
      faster than serial — but the evacuator cannot be trusted (let alone made
      default) until defect 2 is fixed.
  - **Soak — diverse-workload battery, serial + parallel, vs HotSpot** (harness
    `scratch/g1par/g1pause-soak.sh`, `g1pause-confirm.sh`): object trees, held
    graph, primitive arrays, `HashMap`, `String`, pointer churn, and cross-linked
    promotion (`PromoteMixed`). **Serial G1 == HotSpot on every workload that
    completes.** Parallel G1 == HotSpot on every workload *except* the
    `SteadyChurn @16m` corruption above (defect 2). Separately, in the pure
    `--nojit` interpreter some many-frequent-GC runs exceed the 120s watchdog (the
    per-GC `thread::scope` spawn makes parallel slower than serial for tiny CSets)
    — a throughput artifact orthogonal to defect 2 (and to a persistent-worker-pool
    follow-up). The soak's value here was finding defect 2.
- **Step 10 (default flip) — NOT started; still gated.** Most historical gates
  cleared: the marking/mixed-GC repair (`3d067512`) lets G1 reclaim old gen; the
  JIT missed-root **A5 is fixed** (`77c98761`); the self-forward UAF (parallel-evac
  defect 1) is fixed (`7e97111e`); **pause/throughput is now measurable** (p50/p99
  obtainable; CV-G1/CV-Gen throughput 1.07×); and **serial G1 is soak-clean** vs
  HotSpot across a diverse battery. But three things still gate the flip:
  - **OPEN: the parallel-evacuator race (defect 2).** A clean parallel evacuator
    is the prerequisite for parallel-evac-default-on, which is in turn the
    prerequisite for an acceptable large-live-set pause story (serial pauses are
    ~10–20× HotSpot). This is now the #1 G1 work item.
  - **The pause story (§3.3 / §6).** Without a trustworthy parallel evacuator,
    CV-G1's large-live-set pause is ~10–20× HotSpot; a default flip would need
    either parallel-evac-default-on or an explicit "selectable, pause-target
    best-effort" framing.
  - **The blast radius (§6).** Flipping the default changes behaviour for every
    app and every test that assumes Generational; per §4 step 10 it must be **its
    own change with a full suite re-run**, not bundled with this branch. (The
    gauntlet daemon e2e is also still gated on 3 tracked *non-G1* upstream bugs,
    Step 8 item 5.)
  Recommendation: **keep Generational the default.** Land this branch (the µs pause
  sink) as the Step-8 finish; the next G1 work is fixing parallel-evac defect 2,
  after which parallel-evac-default-on + a full-suite re-run can carry the Step-10
  flip as its own change.

---

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
across the gauntlet; the **gpu-bench-cpu G1 SIGSEGV** (Step 8 finding, `task_b53503fd`) is the
current blocker.

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

**Validation (§4 step 9 / §5):** differential — the deterministic benches (e.g. `bintrees18@8g`)
under `CRATONVM_G1_PARALLEL_EVAC=1` must produce **byte-identical checksums** vs serial G1 vs
HotSpot, across worker counts; plus a parallel-evac soak with no leak/corruption. Parallel evac
stays **opt-in** until soak-clean; flipping it on (with a demonstrated large-heap throughput win) is
part of Step 10.

---

## 4. Incremental delivery plan (small, independently-mergeable, each build-green)

Each step compiles and passes existing tests on its own; no step depends on a later step to be
sound.

1. **CLI flag wiring + warning fallback.** Parse `-XX:+UseG1GC`/`-XX:-UseG1GC` → `gc_algorithm`.
   Unknown `Use*GC` → warn + Generational. Unit-test the parser. (No GC behaviour change; G1 only
   runs if explicitly asked.)
2. **Doc truth-up.** Reconcile `ARCHITECTURE.md` vs `README.md`/`CONTRIBUTING.md`: describe
   Generational as default, G1 as opt-in/experimental-but-real, and ZGC-real as built and
   dispatched but compiled in only behind the default-off `zgc` Cargo feature.
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
