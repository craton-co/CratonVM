# H2 TestScript `--nojit` SEGV — findings (2026-06-05)

> # ✅ ROOT-CAUSED & FIXED (2026-06-10, branch fix/blocked-thread-gc-remap-wip,
> # commit 1d4e77e4 + the GcBlockState half swept into dev f3156c00)
>
> **The "additional writer" was the BLOCKED-THREAD GC MAINTENANCE GAP — five
> stacked defects, all around threads excluded from the STW barrier:**
>
> 1. **Stale root_snapshot scanned as GC roots.** A thread parked in a
>    blocking native (`Object.wait`/`Thread.join`/`LockSupport.park`/
>    `ReferenceQueue.remove`) deposits its frame roots once; `update_all_roots`
>    step 11 remapped ONLY the initiator's snapshot. After the first missed
>    moving GC the blocked thread's snapshot pointed into a vacated semispace;
>    once that space cycled back to from-space, the collector EVACUATED
>    GARBAGE through the stale addresses (forwarding-state writes into the
>    interior of innocent live objects) — the heap-side writer.
> 2. **Frames never remapped for missed GCs.** `check_post_block_gc` applied
>    the pointer map only when an STW was active at the exact wake instant;
>    the `gc_generation` counter built to detect missed GCs had zero
>    consumers. Waking threads resumed on recycled from-space addresses —
>    the consumer-side stale receiver faulting in `get_field`. All four
>    crash "faces" (0x1 / 0x4 / 0x6 / 0x100000000) decode as ONE mechanism:
>    a field cell is [disc u32][payload32 u32][payload64 u64] and objects
>    whose bases differ 8 mod 16 have mutually +8-shifted cell grids, so a
>    stale-receiver getfield over re-allocated memory reads a neighbor
>    cell's [disc|payload32] as the pointer — Long→0x1, Object(None)→0x4,
>    Uninitialized→0x6, Int(1)→0x100000000. No off-grid WRITE required.
> 3. **`ReferenceQueue.remove` polled a raw captured `this` inside the
>    blocked region** (acknowledged in the avrora-era comment, band-aided by
>    the num_fields<2 guard). With 1+2 fixed, the guard itself faulted on
>    the stale header — PDB-symbolized smoking gun:
>    `object_num_fields ← native_rq_poll (reference.rs:392) ←
>    native_rq_remove_timeout` on the Common Cleaner thread. Its splice
>    writes (head / referent=Object(None) / size) through stale-but-mapped
>    receivers are the MVStore "Cannot invoke compareAndSetRoot on null"
>    silent-null writer.
> 4. **Registry `java_thread_obj` mirrors + the `unpark(Thread)` reverse
>    index were never scanned nor remapped** — natives (`Thread.enumerate`,
>    `getAllStackTraces`) resurrect collected/stale mirrors into bytecode
>    (the "Stale pointer detected in invokevirtual receiver (all-zero
>    header) — falling back to CP class java/lang/Thread" flood), and
>    unpark lookups by the relocated address silently MISS (lost wakeups).
> 5. **The barrier's "harmless over-count" was not harmless.** `request_stw`
>    excludes blocked threads via a racy counter read; a thread waking just
>    after the read called `arrive_and_wait`, inflating `arrived` for a
>    pause that never counted it — releasing `wait_for_all` while a counted
>    mutator still ran ⇒ the moving collector raced live frames
>    (nondeterministic bootstrap stalls once the rq loops cycled
>    blocked↔expected at ~100Hz).
>
> **Fix (all general, ungated):** per-thread `GcBlockState`
> (`in_blocked_region` flag + composed `orig→cur→new` fixup map, shared
> JvmThread↔registry like root_snapshot); GC initiators call
> `ThreadRegistry::fold_pointer_map_into_blocked` under STW (update_all_roots
> step 20) — remaps blocked snapshots in place + composes every missed GC's
> map into the per-thread fixup; `check_post_block_gc` drains in-flight STWs
> then applies+clears the fixup (frames, monitor_on_exit, pins, printed,
> scoped values) and RE-DEPOSITS the snapshot (the old clear hid a waking
> thread's roots); `deposit_root_snapshot` also pushes `monitor_on_exit`
> (static-synchronized monitors live in no local); the rq remove loops poll
> OUTSIDE the blocked region (an expected mutator cannot observe a GC
> completing mid-poll) with an exponential-backoff park (200µs→10ms — a hot
> yield-spin was a ~10x bootstrap slowdown) and re-sync their local args via
> `end_blocking_region_refs`; registry mirrors are rooted while alive
> (roots step 10b) and remapped + reverse-index re-keyed per GC
> (`update_thread_objs_after_gc`, gc step 21); blocked-region transitions
> are serialized under the barrier's inner lock and the leave side WAITS OUT
> any active STW while still counted blocked (exact arrivals — no
> over-count, no mutator/GC race). Diagnostics: `CRATONVM_DBG_BLOCKGC=1`
> prints fold/wake activity.
>
> **Verification:** the deterministic pre-fix crash (6/6 runs, read at
> 0x100000004, RVA 0x1D8549) is gone — post-fix runs progress far past the
> historical SQL-5219 crash cluster with no SEGV; each fix layer shifted the
> failure signature exactly as predicted (wild-small receiver → unmapped
> stale receiver in the rq guard → stale-Thread WARN flood → clean). SEGV
> verification on the converged dev build: 2× 2400 s runs + 1× 600 s
> capture run, ZERO crash signatures (pre-fix: 6/6 deterministic SEGV in
> 84–229 s).
>
> ## ✅ BLOCKER 2 FIXED (dev f0a44501): the queryGroup pseudo-hang was a
> ## BigDecimal comparison-cycle from the active precision natives
>
> Post-SEGV-fix runs stalled ~35 s in, watchdog-pinned to
> `Select.queryGroup → gatherGroup → SelectGroups$Grouped.nextSource →
> SessionLocal.compare` (main-vm hot in the interpreter, all other
> threads healthy). Recon (4-agent sweep, 2026-06-10): the GROUP BY key
> map is `new TreeMap<>(session)` — serviced by the NATIVE TreeMap
> shadow in comparator/array mode (`native-collections` `tm_binary_search`
> → `comparator_compare` → interpreted `SessionLocal.compare`; explains
> the missing TreeMap frames). The comparison itself was corrupted by the
> ACTIVE real-JDK-build BigDecimal natives — NOT the doc's old f64
> add/sub/mul theory (those were already exact):
> `native_bd_precision` returned the raw lazy-0 slot (JDK's "not yet
> computed" sentinel) into real `compareTo`'s compareMagnitude
> adjusted-exponent quick exit, and `bd_alloc` overcounted precision for
> |v|<1 ("0.05" → 3, true 1) — together provably producing strict
> comparison CYCLES (a<b<c<a) over H2 DECIMAL values → TreeMap binary
> search misses existing keys → duplicate group per row → O(n²) with
> deep interpreted compare chains = the "hang" (the INVOICE
> DECIMAL(10,2) block right after the SQL-6554 marker). Plus a
> negative-scale 1000× value inflation (`apply_scale` round-trip) and
> f64-based signum (also shadowed inside real compareTo). **Fix (dev
> f0a44501):** `bd_unscaled_and_precision` (JDK-true precision +
> negative-scale-aware unscaled recovery) in both allocators;
> `native_bd_precision` computes-and-caches on the 0 sentinel; exact
> negate/abs/signum via the unscaled BigInt. Measured: stall point moved
> ~24 s/10.1 KB stderr → ~201 s+/14.8-34.8 KB, the 6 deterministic
> line-5219 mismatches → 0.
>
> ## ⛔ BLOCKER 3 (current): class-resolution RwLock deadlock,
> ## nondeterministic, several minutes in
>
> With blockers 1+2 fixed, runs progress much further then either stall
> or (less often) SEGV at a new face (release RVA 0x7A34E5, unmapped
> heap-shaped read 0x86DA8E08, main-vm). The stall is now a REAL LOCK
> DEADLOCK — fully symbolized census in
> `apps/h2database/h2/h2-stall2-stacks.txt` (2026-06-10, rwd build):
> - main-vm: `resolve_field_ref` → `RawRwLock::lock_exclusive_slow`
>   (wants WRITE);
> - Thread-2: `execute_string_concat` (indy) → `alloc_java_string_object`
>   → `RwLock::write` (second queued WRITER);
> - Thread-1: `populate_virtual_invoke_cache` → `resolve_method_ref`
>   → `lock_shared_slow` at +0x5c (the function's FIRST lock — queued
>   READER behind the writers);
> - Thread-3/Thread-4: parked in Java `Condition.await`
>   (`native_cond_await` → `monitor_wait`) — one of them almost
>   certainly entered the park while a caller frame still HELD the
>   contended guard (the original "don't hold class_manager across the
>   re-entrant invoke" warning — `resolve_method_ref`'s read_recursive
>   at interpreter.rs:16529 protects only same-thread recursion, not
>   guard-across-park).
> ### ✅ Blocker 3 RESOLVED in three commits (2026-06-10, all on dev):
> - **39aaffe3** — the ABBA itself: `resolve_field_ref`'s own-fields path
>   took `resolution_cache.write()` INSIDE its live `class_manager.read()`
>   block (cm→rc), while `execute_invokedynamic` held
>   `resolution_cache.read()` across the whole cached-call-site execution
>   (StringConcat allocates → `alloc_java_string_object` takes
>   `class_manager.write()` for `load_class("java/lang/String")`: rc→cm).
>   Fixes: drop cm before the cache write (mirroring resolve_method_ref's
>   documented rule); clone the cached site out of the rc guard (cheap —
>   Arc<str> fields) before executing.
> - **f080908b** — the STW triangles the lock-order audit found: contended
>   `monitorenter` and class-init waiters parked WITHOUT GC-blocked
>   marking (counted in `expected`, never arriving → owner-parked-at-
>   safepoint + contender + GC = three-way wedge). New
>   `MonitorTable::enter_or_contend`/`Monitor::try_enter`/`block_enter`
>   split + `monitor_enter_blocking` blocking-site protocol; class-init
>   waiter wrapped in the full protocol; `resolve_inline_site` (JIT
>   inlining) phase-split so resolve_field_ref runs with no cm guard held
>   (was a same-thread self-deadlock on a cold field class).
> - **efd17806** — protocol-safety rework after the first verify run
>   panicked ("monitor inflation invariant"): a GC completing during the
>   GC-blocked contended wait left the caller's raw Rust `obj`/args copies
>   stale/dead. The monitorenter arm now PINS obj in native_pin_roots
>   (alive + remapped, popped back fixed); the sync-method prologue and
>   `ctx.monitor_enter` deliberately REVERTED to expected-mutator blocking
>   (their callers hold immutable unrooted `&[Value]` arg slices that
>   cannot be remapped — the residual rare wedge there is documented; the
>   proper fix needs interpreter-level STW participation that can remap
>   the args).
>
> Verified: no deadlock, no panic across post-fix runs; progression
> 24 s (pre-BD-fix) → 201 s → 241 s-crash → 6+ min of clean work. NOTE
> the long FLAT stderr phases are REAL WORK: testScript.sql:5721 is
> `select var(...) from system_range(1, 1000000)` — million-row
> aggregates take minutes interpreted; full-script completion needs a
> 1-2 h timeout (soak item).
>
> ### ⛔ REMAINING (the last known face): worker-thread stale-receiver
> SEGV, read at 0x13 (payload 0x3 + 0x10 header = off-grid `Double`-cell
> read through a stale receiver, per the §face-decoder), "Thread-6",
> ~2/3 of runs, 4-6 min in (post-aggregate region, last marker SQL
> ~5725). Excluded: Long-smuggle asymmetry (CRATONVM_DBG_LONGROOT
> fires 0× in H2, strict mode changes nothing); cleaners (NO_CLEANERS
> crash persisted pre-fix); the blocked-thread gap (fixes verified
> active). CRATONVM_DBG_BADRECV run: 0 hits in 900 s (perturbs timing)
> but exposed a SEPARATE lead — the aggregate region floods
> `gen_heap::get_field: out-of-bounds field read dropped` WARNs at
> ~millions/min (19 GB stderr in 15 min!): a per-row speculative
> layout probe misfiring (receiver valid, index past layout) — both a
> perf drag on exactly the slow region and a possible cousin of the
> stale-receiver face. Next: identify the OOB-flood probe (which caller
> probes fld[0..3] on 0-slot java/lang/Object receivers per aggregate
> row), then MEMWATCH/instrumented get_field for the 0x13 face.
> Verification recipe unchanged: `run-h2-verify.ps1 -Runs 5
> -TimeoutSec 2400`+ + the apps suite (2026-06-10 run: zero regressions
> vs baseline, elasticsearch-version FAIL→PASS, wildfly-health
> known-FAIL mode change attributed to the concurrent session's WildFly
> boot-latch work).
>
> Historical analysis below: the ReferenceProcessor re-emission (fixed at
> dev a9bc91f6) was the FIRST writer; the superseded status + hunt log
> follow.

> ## (superseded 2026-06-10) ⚠ PARTIALLY RESOLVED — the bc-math-ec `0x4` / silent-null corruption is
> FIXED (dev a9bc91f6), but the H2 SEGV below PERSISTS on the fixed build
> (re-verified 2026-06-10: 5/5 runs crash) — H2 has an ADDITIONAL
> stale-receiver writer beyond the ReferenceProcessor re-emission
>
> **Root cause: the `ReferenceProcessor` re-emitted every cleared/enqueued/
> cleaner action on EVERY GC, forever** (`pending_queues` read
> non-destructively; `cleared_ref_objects()` idempotent; cleaner
> `cleared`-flag rescan; `finalization_queue.iter()`). The post-GC consumer
> (`process_references_after_gc`, interpreter.rs) WRITES heap fields per
> emission (referent null = 16-byte `Object(None)` to fld[0]; queue
> head/size/next). Registry addresses are only single-step remapped per
> cycle, so once an emitted Reference/queue DIED, its address stuck in the
> registry forever; when the young allocator recycled that memory for a live
> object which later MOVED, the stale address became a pointer-map KEY and
> the per-GC re-emission wrote into the innocent object through a
> "legitimately remapped" address — defeating every receiver-side guard by
> construction. On-grid landings = perfectly legal nulls (the scanner-
> invisible "Cannot invoke isZero/subtract on null" BigInteger NPEs);
> interior landings = the mis-gridded `{disc=4, payload=0}` → the famous
> `Object(Some(0x4))` + zeroed next word (hexdump-proven). Explains the
> GC-pressure correlation, random victims, promotion-survival, and the
> "GC writes it" illusion (the writes happen in the post-GC window, before
> ec_watch's GC-EXIT detect).
>
> **Evidence chain:** hexdump of victim ±128B (mis-gridded `Object(None)`),
> `CRATONVM_DBG_NO_REFPROC=1` exclusion → FixedPointTest **6/6 OK** (first
> passes ever, was 0/40+), `CRATONVM_DBG_NO_CLEANERS=1` bisect → 0/6 (loops,
> not invokes), once-only fix → **8/8 OK default-mode** + regression pool:
> zero regressions and commons-math-junit-probe FAIL→PASS.
>
> **Fix (gc/src/reference.rs + interpreter.rs):** exactly-once emission per
> Java semantics — `take_newly_cleared()` (flags `clear_emitted`),
> `pending_queues` drained on emission, `finalization_queue.drain(..)` (also
> fixes a double-finalize), cleaner `action_emitted` flag. Defense-in-depth
> kept: `is_stale_young` guards (young + not-in-map ⇒ dead ⇒ skip) on all
> four consumer loops, `VmHeap::is_in_young_addr` /
> `GenerationalHeap::is_in_young_either`.
>
> **H2 RE-VERIFICATION 2026-06-10 (dev a9bc91f6, post-fix build): the SEGV
> PERSISTS — 5/5 runs FATAL `EXCEPTION_ACCESS_VIOLATION` (walls 84–229s).**
> The ReferenceProcessor fix did NOT cure H2; an additional writer produces
> the same wild-small-receiver corruption. Same site + call shape as the
> historical log below: `GenerationalHeap::get_field` header read
> (`gen_heap.rs:920`) consumed by a `getfield` (interpreter.rs:7222) inside
> a Java method driven by native `register_essential_natives::closure$105`
> → `ctx.invoke_virtual` (full symbolized stack:
> `apps/h2database/h2/h2-rwd-symbolized.log`, hs_err archives alongside).
> Faces: release build is fully DETERMINISTIC — every run reads
> `0x0000000100000004` (receiver payload exactly 2^32) at RVA `0x1D8549`
> on thread "main-vm"; release-with-debug build reads `0x11` (receiver
> `Object(Some(0x1))` + the 0x10 header offset — the historical
> `Object(Some(6))→0x16` face) on "Thread-1". The mismatch cluster at
> `testScript.sql` line 5219 (X'…'/CHAR-padding region) precedes every
> crash. Next: localize with the gated bc-math-ec instruments, all
> in-tree on dev — `CRATONVM_DBG_MEMWATCH=<hexaddr>`,
> `CRATONVM_DBG_SEEDHUNT`, `CRATONVM_DBG_STRAYSTACK`.
>
> Repro gotchas (each cost a false start): the DEFAULT 120s stack-dump
> watchdog (vm-cli/src/main.rs) `abort()`s TestScript mid-run and fakes a
> "clean" non-SEGV exit — set `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`; copy
> the binary to a unique non-`cratonvm*` name first (cross-session
> `taskkill /F /IM cratonvm.exe` sweeps); `CRATONVM_SYMBOLIZE` is an
> offline RVA-symbolizer (prints symbols and EXITS — not a runtime gate):
> crash first, then feed the hs_err RVAs back through the SAME binary
> (release profile has no usable symbols; use release-with-debug).
> Scripts: `apps/h2database/h2/run-h2-verify.ps1` (N-run verify loop) and
> `capture-h2-segv-rwd.ps1` (crash + auto-symbolize, release-with-debug).
> Sections below are the historical hunt log.

> ## ⚡ UPDATE 2026-06-09 (bc-math-ec side, worktree `CratonVM-ecgc`, branch
> `fix/bc-math-ec-gc-0x4`, now == dev) — **pin fix did NOT cure it; the
> corruption is a `long[]` SMEAR, not a single stray Value write.**
>
> ### Measured (FixedPointTest, JIT off, -Xmx128m)
> 1. **`bi_alloc`/`bd_alloc`/`bd_write_into` pins (526edf9b) verified IN the
>    binary → still 0/8 clean runs.** Same outcomes as before (wrong point /
>    NPE null / occasional SEGV). So the pinned natives were NOT the writer
>    (consistent with this doc's "SEGV persists, flood=0").
> 2. **Resolved-victim sweep across 8 runs:** every resolved victim field reads
>    payload EXACTLY `0x4` on EC objects at per-class-stable field indices
>    (X9ECParameters fld[1]/[2], ECCurve$* fld[4], ECPoint/SecT* fld[2],
>    ECFieldElement$Fp fld[1]); victim ADDRESSES repeat across runs
>    (ECCurve$Fp AND ECCurve$F2m @0x20a00fb8 fld[4] in different runs —
>    allocation layout is deterministic).
> 3. **`CRATONVM_DBG_MEMWATCH` (new, vm/src/runtime/memwatch.rs): O(1)
>    absolute-address watch** polled at every safepoint + native return +
>    post-GC. Armed on the repeat victim payload (0x20a01028): **flip
>    `0x0 -> 0x1c` caught at a safepoint inside
>    `LongArray.addShiftedUp([JI[JIII)J` ← `modMultiply` ←
>    `ECFieldElement$F2m.multiply` ← `ECPoint$F2m.add`** — i.e. an interpreted
>    F2m long[]-arithmetic store wrote a small long to the watched address
>    while its surroundings read as raw math data (the "cell disc" at -8 was
>    garbage, not a Value disc).
> 4. The same runs show **massively smashed young HEADERS** ("GC: inconsistent
>    header — kind=Object array_length=2561 num_slots=942662 class_id=0", many
>    variants) and earlier whole-REGIONS of small values at stride 8
>    (`<unresolved>` arr[9..991]).
>
> ### Reframe (supersedes "stray 16-byte Value write off-by-8")
> An interpreted **`lastore` loop writing through a STALE (GC-moved) `long[]`
> reference** sprays dozens of math longs over the young heap: headers become
> garbage (the WARNs), reference-field payloads become small longs — `0x4`,
> `0x6`, `0x1c`, `0x3` are just F2m bit patterns (`0x4` was over-interpreted as
> the Object discriminant; the H2 `0x6` likewise needs no Uninitialized-disc
> story). `set_array_element` bounds-checks against the GARBAGE header at the
> stale address (random `array_length` parses huge) so the writes pass.
> Candidate stale-ref source: the **jobject-as-Long smuggle** — an invoke
> return carrying `[J` as compact long bits, `astore`d into a local →
> `LKIND_LONG` → **`scan_local_objects`/`update_local_refs` SKIP it** (the
> 2026-06-04 collision-long fix) → never remapped after a young GC → stale.
> Note the tension: that skip is REQUIRED for genuine collision longs; the fix
> must be at the smuggle (a `[J`/`[I` value must never sit in a local AS Long),
> not by re-rooting LONG-kind slots.
>
> ### New gated instruments (all default-OFF, committed on the branch)
> - `CRATONVM_DBG_MEMWATCH=<hexaddr>` — the absolute-address watch (above).
> - `CRATONVM_DBG_ARRSTORE` — `Lastore`/`Iastore`-family receiver-header
>   validation at the write (raw kind/elem/array_length bytes); dumps receiver
>   + Java stack on a garbage-header receiver = catches the smear at store #1.
> - `CRATONVM_DBG_STALELONG` — at GC remap, logs any LONG-kind local whose raw
>   bits match a `pointer_map` key, with the owning method (smuggled-ref
>   candidates; collision longs also match — discriminate by method).
> - **Verdicts (all measured, 3+ corrupting runs each): NEGATIVE — do not redo.**
>   `[arrstore]`=0 (no interpreted Xastore through a GARBAGE-header receiver),
>   `[stalelong]`=0 (no LONG-kind LOCAL matching a moved object at remap),
>   `[longroot]`=0 (the value_stack O1-hybrid Long|Double loose-rooting branch
>   NEVER fires in this workload) and `CRATONVM_LONGROOT_STRICT=1` changes
>   nothing (0/4). Also note: corrupting runs occur WITH ZERO header-smear
>   WARNs (subtle mode: wrong point / NPE-null only) — the big smear is a
>   sometimes-symptom, not the constant.
> - Note the latent (unrelated-to-this-bug?) asymmetry found while testing:
>   `ValueStack::scan_object_refs` roots `CompactTag::Long`-tagged slots via
>   loose `is_heap_addr` (O1 hybrid, WildFly smuggle) but
>   `update_object_refs` has NO Long arm — a rooted Long-tagged smuggle goes
>   stale on every move. Latent because the branch never fires here.
> - REMAINING live theory: a stale array ref pointing at a REUSED region that
>   now holds ANOTHER plausible array (kind=1 header → every validator passes,
>   bounds-checked against the wrong array's length). Discriminator in flight:
>   memwatch HIT dump now includes the top frame's locals + array extents — if
>   the watched (corrupted) address falls INSIDE the frame's `[J` receiver
>   extents the write is legit-but-relocated (theory dead too); if OUTSIDE,
>   the stale/OOB receiver is proven with its exact geometry.
>
> ### ⚡ HEXDUMP BREAKTHROUGH (one-shot dump at [small4] detection)
> Victim `java/util/logging/Level.fld[4] -> 0x4` captured with ±128 bytes
> (full dump in session log; reproduce with `CRATONVM_DBG_RSET_AUDIT=1`):
> **victim payload word = 0x4 and the IMMEDIATELY FOLLOWING u64 = 0x0 (the
> neighbour Level's class_id word, zeroed)** — i.e. a verbatim 16-byte
> `Value::Object(None)` `{disc=4, payload=0}` written **8 bytes off the cell
> grid**. The corruption is a NULL-REFERENCE FIELD WRITE through a wrong/stale
> receiver. Geometry: write target = stale_obj+40 (field 0) with
> stale_obj = victim+0x48 (interior). CRITICAL COROLLARY: when the same stray
> write lands ON-grid (~50%), it produces a PERFECTLY LEGAL null — invisible
> to every small-value scanner — explaining the "Errors: NPE-on-null with
> small4=0" runs. The 0x4 face is just the off-grid half.
>
> ### Fix landed (correct + verified firing, but NOT sufficient) + verdict
> `process_references_after_gc` writes (referent clear `Object(None)`,
> enqueue head/size/next, finalize/cleaner submits) now SKIP any pre-GC
> address that is in EITHER young semispace and NOT a pointer-map key — the
> precise "did not survive" criterion (new `VmHeap::is_in_young_addr` /
> `GenerationalHeap::is_in_young_either`). The old `num_fields < 2` guard was
> too weak (phantom num_slots >= 2 passes). Logs `[refproc] SKIP dead ...`
> under `CRATONVM_DBG_STRAYSTACK`. **Verdict: guard fires 159–1433×/run, yet
> 0/8 runs pass — BigInteger-null NPEs persist. So refproc's loops are not
> the (only) Object(None)-writer.** (Possible confound to check: are the
> 1433 "dead" skips genuinely dead, or is the ref registry's relocation
> (`ReferenceProcessor::update_after_gc`, called from memory/gc.rs:206 via
> update_all_roots AFTER process_references_after_gc) skipped/partial in some
> path, making live refs look dead by one stale generation?)
>
> ### In flight: subsystem exclusion (`CRATONVM_DBG_NO_REFPROC=1`)
> Skips process_references_after_gc + run_finalizers + run_cleaner_actions
> entirely. Corruption persisting ⇒ the whole reference/finalizer/cleaner
> subsystem is exonerated in one experiment; stopping ⇒ writer is inside it
> (next suspects there: cleaner/finalizer Java invokes on stale queued
> addresses — `Cleanable.clean()` unlinks with null writes through `this`).
> Next writers to hunt if exonerated: any other `Object(None)`-through-
> possibly-stale-receiver path (interpreter ReferenceQueue poll/remove
> helpers ~interpreter.rs:530-690; JNI SetObjectField; exception-init paths).
>
> Also: dev's scripts cleanup deleted `build-wt.bat`; recreate it (vcvars +
> unset VCINSTALLDIR/VSCMD_ARG_TGT_ARCH/CARGO_TARGET_DIR + cargo build) — see
> the memory file. The earlier `[refproc]`/reference-processing stale-ref fix
> (merged) stays valid (OOB-flood 1-13 → 0) but is unrelated to this smear.

Companion to `docs/bc-math-ec-gc-0x4-handoff.md`. **The H2 `org.h2.test.scripts.TestScript`
`--nojit` FATAL `EXCEPTION_ACCESS_VIOLATION` is a SECOND reproduction of the bc-math-ec
`0x4` GC corruption** — same mechanism, different app/payload.

## The SEGV == bc-math-ec `0x4` GC corruption

Symbolized with `release-with-debug` + the new `file:line` crash symbolizer:

- Faults in `cratonvm_gc::gen_heap::GenerationalHeap::get_field` (`gen_heap.rs:920`, the
  header read) on a wild receiver `Value::Object(Some(ptr=6))`.
- **Faulting read at `0x16` = `6 + 0x10`** — identical signature to bc-math-ec
  (`Object(Some(0x4))` → SEGV at `0x14` = `4 + 0x10`). Small `Object` payload + a read at
  `payload + 0x10` (the `num_slots` header offset).
- Consumed by a `getfield` inside a Java method driven by native
  `cratonvm_native_builtins::register_essential_natives::closure$105` → `ctx.invoke_virtual`.
- Clusters at `testScript.sql` ~line 5539 (the DECIMAL-arithmetic region), GC-pressure-driven.
- Under `--nojit`, `gc_quiescence::is_active()` is always false (JIT-only), so the **moving
  Cheney young collector** runs — the collector the `0x4` hunt blames.

This is the deep, unsolved bug being worked in worktree `CratonVM-ecgc`. H2 is a cleaner,
higher-pressure repro than `FixedPointTest` (corrupts at `-Xmx1g`; `FixedPointTest` needs
`-Xmx256m`). Repro: `repro-h2.bat` (env `XMX`/`TMO`), or:
`target/release-with-debug/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --nojit -Xmx1g -cp ".;temp" org.h2.test.scripts.TestScript` from `apps/h2database/h2`.

## FIXED here (a SEPARATE real bug — NOT the SEGV)

`bi_alloc` / `bd_alloc` / `bd_write_into` (`native-builtins/src/lib.rs`) had the **same
native use-after-move** the handoff already fixed for `bi_alloc_int` (fact 7): allocate a
BigInteger/BigDecimal, then call a nested allocator (`bi_alloc` / `new_array` /
`create_string`) that can young-GC and relocate the not-yet-rooted object, then `set_field`
through the STALE ref. Fixed by pinning across the nested alloc
(`pin_native_root`/`read_native_pin`/`unpin_native_roots`).

- **Result:** the `set_field` OOB-flood (`class=java/lang/Object num_slots=0`) dropped
  **181 → 0** at the crash region. **The wild-ref SEGV PERSISTS** (flood=0) → the OOB-flood
  and the SEGV co-occurred but are DISTINCT; the SEGV is the deep GC bug, not this.
- **This very likely also helps bc-math-ec** — that handoff fixed only `bi_alloc_int`,
  leaving these three siblings unpinned.
- Status: UNCOMMITTED; built into `target/release-with-debug` only (rebuild `target/release`
  to ship). Needs a regression-pool pass before merge (BD is widely used).

## Separate HANG (the "nondeterministic hang")

Once the BD fix lets H2 worker threads stay alive, a **write-preferring `parking_lot::RwLock`
deadlock** on the class-metadata/resolution locks surfaces (cdb-localized, `cdb_hang.ps1` →
`cdbdump.log`): main-vm `initialize_class_shared` → `resolve_field_ref` → `lock_exclusive`
(WRITE, class-load) vs worker threads (via #105 `invoke_virtual`) → `resolve_method_ref` →
`lock_shared` (recursive READ). Fix direction: `read_recursive()` for the re-entrant
`class_manager.read()`, or don't hold it across the re-entrant `invoke_virtual`.

## Mismatches (~40, lower priority)

- **f64 BigDecimal arithmetic:** `native_bd_add/subtract/multiply/negate` compute via `f64`
  (`format!("{}", a + b)`) → scale + precision lost (`10*1.00`→`10`, `-1.00`→`-1`). Real fix:
  decimal arithmetic on unscaled-int + scale, not f64.
- **`CharBuffer.allocate`** (`charset.rs:203`) returns an abstract base `java/nio/CharBuffer`,
  so H2's real `charBuffer.compact()` bytecode → `AbstractMethodError`. Real fix: real
  `HeapCharBuffer`.

## Tooling added (all gated / standalone)

- Crash symbolizer now emits `file:line` (`crash_handler.rs` `SymGetLineFromAddrW64`).
- `CRATONVM_DBG_BADRECV` — logs Java frames + field + Rust backtrace on a non-heap getfield
  receiver and raises NPE instead of faulting (note: its per-getfield check perturbs timing
  toward the deadlock, so it tends to hang before reaching the SEGV region).
- `symbolize-crash.sh`, `cdb_hang.ps1`, `repro-h2.bat`, `DecChurn.java`.
