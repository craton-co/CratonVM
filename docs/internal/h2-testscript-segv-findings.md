# H2 TestScript `--nojit` SEGV — findings (2026-06-05)

> # ✅ RESOLVED 2026-06-10 — root cause found and fixed (bc-math-ec `0x4` /
> silent-null corruption; very likely this H2 SEGV too — re-verify H2)
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
> H2 TestScript + the `--nojit` SEGV below should be re-verified against the
> fixed binary; the mechanism matches (DECIMAL-region reference churn).
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
