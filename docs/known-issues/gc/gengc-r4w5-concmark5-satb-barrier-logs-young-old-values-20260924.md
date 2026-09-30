# The generational SATB barrier logs young old-values that every consumer drops

> **STATUS (2026-09-29, gce e1/x): KEEP -- perf page, unmeasured.** `w1_satb_old_only` (`CRATONVM_GEN_SATB_OLD_ONLY`) = HotSpot on every e1 battery (correctness only). **Remaining:** the A/B measurement that decides flip or "not worth it".

> **STATUS (2026-09-29, gce e1/c): soundness re-read, unchanged -- a PERF page awaiting its measurement.** A young old-value is either a snapshot young object (its old referents were seeded at the initial mark) or post-snapshot, and a young object promoted during the cycle is not sweep-eligible, so dropping it loses no snapshot edge, with the TAMS switch too. The d5/r measurement block decides.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- PERF page, opt-in filter (`CRATONVM_GEN_SATB_OLD_ONLY`) landed; only the measurement decides.** The filter stays correct on its battery row (`GenR4W5DirectOldGrowthProbe -Xmx512m` with the flag and the service thread, `w1_satb_old_only`, =HS). The conc9 measurement (`Y/D` above 1/2 and no wall-clock loss, arms interleaved) was not in the d7 battery.

## STATUS (2026-09-28, gcd d5/r): soundness RE-VERIFIED against d3/d4; a PERF page with the opt-in filter landed; only the measurement below decides (flip, or retire as "not worth it")

- **Soundness vs d3/d4:** d4/n's true-root seed and Phase 5 veto are
  stop-the-world old-gen code and read no SATB entry; d4/j's second major
  likewise. The argument below (every old object reachable at the initial
  mark through a young object is seeded at that pause, so dropping a YOUNG
  old-value loses no snapshot edge) is unchanged; it holds for both seeds
  (`collect_young_to_old_roots`, and the live young set under
  `CRATONVM_GEN_Y2O_LIVE_SEED`).
- **The measurement (Linux, one binary, arms interleaved, 3 rounds):**
  ```
  for i in 1 2 3; do for arm in "" "CRATONVM_GEN_SATB_OLD_ONLY=1"; do
    /usr/bin/time -f '%e s' env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
      -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 4 2>&1 \
      | grep -E 'steady-promotion|conc_driver:| s$'
  done; done
  ```
  Expected: both arms `steady-promotion threads=4 ring=150000 iters=3000000
  checksum=558000555608192`; default arm `concdrv_cycles_started>=1` and
  `D=concdrv_satb_drained_in_mark>0` (else inconclusive), read
  `Y=concdrv_satb_outside_old_gen`; flag arm `concdrv_satb_outside_old_gen=0`.
- **Decision:** flip (`gen_satb_old_only` in `types/src/flags.rs` from
  `non_empty_non_zero` to `on_unless_zero`, the `types` default assertion
  `assert!(!d.gc.gen_satb_old_only)` inverted, and the two test edits in the
  wave-6 section below) when `Y/D > 0.5` in every round and the flag arm's
  median wall time is not worse; otherwise retire the page as "measured, not
  worth it". Owner: orchestrator.

## STATUS (2026-09-27, gcd d2/g, superseded above): soundness RE-VERIFIED against gcd d1/c and d2/g; still the one measurement below (unchanged); no code change

Re-read against what landed since the conc9 block:
- **d1/c `mark_young_to_old_refs` young-instance → loader seed** (and d2/g's
  unparseable-stretch widening of it): a STOP-THE-WORLD old collection's
  seed, taken from a walk of young from-space; it reads no SATB entry.
- **d1/c concurrent sweep reference-row drop** (`gen_conc_sweep_drop_reference_rows`):
  keyed on the spans the sweep frees and the registration sequence; no SATB
  entry involved.
- **d1/c `young_reached_only_through`** (remark, under
  `CRATONVM_GEN_Y2O_LIVE_SEED` + the hook): a live young marking from the
  remark's roots and old holders; it reads no SATB entry.
- **d2/g precise-root promotion** (opt-in): changes which young objects a young
  pause promotes, not what the barrier logs. A root value promoted by it is an
  old object from then on and is logged like any other old old-value.
- The argument itself still holds: every old object reachable at the initial
  mark through a young object is seeded at that pause (all-young walk, or the
  live young set under `CRATONVM_GEN_Y2O_LIVE_SEED`), so dropping a YOUNG
  old-value loses no snapshot edge.

The flip decision is unchanged: the conc9 measurement below (`Y/D` above one
half and no wall-time loss on the flag arm). Owner: orchestrator (opt-in flip).

## STATUS (2026-09-27, gen r5w5/conc9, superseded above): NARROWED to one measurement; no code change (the filter stays opt-in until it is measured); soundness re-checked against the features added since

- **Nothing to fix in code.** The filter is landed behind
  `CRATONVM_GEN_SATB_OLD_ONLY` (below). Flipping it is a perf decision with no
  measurement yet, and the lane rules keep unmeasured perf changes opt-in.
- **Soundness re-checked against what landed after the filter** (r5w3 / r5w4 /
  this wave). None of them reads a YOUNG SATB entry:
  - class unloading (`CRATONVM_GEN_CONC_CLASS_UNLOAD`): a young instance's
    loader is a root at both pauses (`gen_conc_young_instance_loaders`), and a
    young loader's rows are marked by `ConcurrentMarker::mark_rows_of_live_owners`
    (it is never sweep-eligible). A dropped young old-value loses no edge;
  - remark reference processing: a young `Reference` is never in the skip set
    (`set_reference_skip` ignores non-old addresses), so its referent stays
    traced through `collect_young_to_old_roots`;
  - the dead-finalizer retention and its recorded closure (this wave): it
    starts from published OLD candidates only.
- **Why the default-arm count is the whole decision.** Phase 2 counts the young
  entries it drains (`concdrv_satb_outside_old_gen`) against all drained ones
  (`concdrv_satb_drained_in_mark`). Concurrent cycles now run by default from
  compiled code (the allocation-failure door), so the service thread is no
  longer needed to get a non-zero denominator. Exact runs (interleave, 3 each):

  ```
  for arm in "" "CRATONVM_GEN_SATB_OLD_ONLY=1"; do
    /usr/bin/time -f '%e s' env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
        -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 4 2>&1 \
      | grep -E 'steady-promotion|conc_driver:| s$'
  done
  java -XX:+UseSerialGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 4
  ```

  Expected:
  - both arms: the `steady-promotion` stdout equal to HotSpot's;
  - default arm: `concdrv_cycles_started>=1`, and `Y=concdrv_satb_outside_old_gen`,
    `D=concdrv_satb_drained_in_mark` with `D>0` (else inconclusive);
  - flag arm: `concdrv_satb_outside_old_gen=0`.

  **Flip** (`gen_satb_old_only` default in `types/src/flags.rs`, `off_word
  Some("0")`, plus the two test edits in the wave-6 section below) when `Y/D`
  is above one half and the flag arm's median wall time is not worse.
  Otherwise retire the page as "measured, not worth it".

## STATUS (2026-09-26, gen r5w1/refs5, superseded above): filter LANDED opt-in (`CRATONVM_GEN_SATB_OLD_ONLY`), unbuilt; no test had to change

- **The diff below, behind a flag.** In `GenerationalHeap::satb_barrier`, after the
  `Value::Object(Some(r))` match, and only while the queue is active:
  `if gc_flags().gen_satb_old_only && addr.wrapping_sub(card_table.base_addr()) >= card_table.region_size() { return; }`,
  where `addr` is the old value's address.
  That is one flag load (the `OnceLock` probe `gc_flags()` always is), one subtract
  and one compare. The card table spans exactly the old generation's reservation
  (`CardTable::new(old_gen.base_ptr(), old_gen_size)`), so the test is exact.
- **Why a flag, not the unconditional diff.** It is a perf change with no
  measurement yet. The concsvc6 count (`concdrv_satb_outside_old_gen` against
  `concdrv_satb_drained_in_mark`) was never read. The flag also keeps both pinned
  tests unchanged (`w5e_one_satb_gate::the_generational_barrier_gates_on_the_queue_not_the_phase`
  and `vm/src/jit/helpers.rs::jit_putfield_object_satb_pre_barrier_enqueues_old_ref`,
  whose old-values are young), so no cross-lane test edit is needed. The flip is
  a one-line change plus those two test edits (the diffs below).
- **Every emitter is covered:** the interpreter, the JIT pre-barrier's slow path,
  JNI and the `Reference.get()` keep-alive all call `satb_barrier` (re-checked).
  Dropping a YOUNG keep-alive value is sound: a young referent is never swept by
  the old-gen cycle, and its old-gen children are remark roots through
  `collect_young_to_old_roots`.
- **Test (unrun):** `gen_heap::tests::w5e_one_satb_gate::the_old_only_filter_drops_young_old_values_and_keeps_old_ones`.
- **Probe (count only; stdout must match):**

  ```
  for arm in "" "CRATONVM_GEN_SATB_OLD_ONLY=1"; do
    env $arm CRATONVM_GEN_CONC_SERVICE_THREAD=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
        -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 4 2>&1 \
      | grep -E 'steady-promotion|conc_driver:'
  done
  ```

  Expected:
  - **Default arm:** `concdrv_satb_outside_old_gen=Y` with Y >= 0. This is the
    measurement this page asked for; read Y / `concdrv_satb_drained_in_mark`.
  - **Flag arm:** `concdrv_satb_outside_old_gen=0`.
  - **Both arms:** the same `steady-promotion` stdout as HotSpot.

  The service thread is in the command because, without it, a compiled program
  opens no cycle (`../../internal/gc/gengc-r5w1-refs5-concurrent-cycle-never-starts-from-compiled-code-FIXED-20260927.md`),
  and the counts would all be 0. Flip the default if Y is a large share and the
  wall time is no worse.

*Filed 2026-09-24 by gen round 4 wave 5, lane `concmark5`. Status: **OPEN**. Split out of
`docs/internal/gc/gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`, item 5,
first bullet: the one item of that page that is not fixed. The page was retired around it.*
*Severity: **perf** (barrier volume while marking). No correctness claim.*

## Code location

- `gc/src/gen_heap.rs::GenerationalHeap::satb_barrier`: logs every non-null reference
  old-value while the SATB queue is active, young ones included.
- `gc/src/concurrent_mark.rs`: every consumer screens entries through
  `markable_old_object`, which accepts only old-gen object starts. These consumers are
  Phase 2's per-slice drain, `remark`'s drain, and its final quiescing drain.
- `vm/src/jit/helpers.rs::jit_putfield_object_satb_pre_barrier_enqueues_old_ref`: the test
  that pins today's behaviour. It asserts that a YOUNG old-value is logged.

## What is wrong

The generational concurrent marker traces the OLD generation only. A young old-value in
the log costs something at each step:

* a per-thread bucket append on the mutator (the barrier's hot path);
* a spill into a shard every 256 entries;
* a drain;
* a `markable_old_object` rejection in the marker.

It is never marked. Keeping it out of the log is also sound. An old object reachable at
the snapshot only through a young object crossed a young→old edge, and that edge's target
is an initial-mark root (`collect_young_to_old_roots`). A young value promoted mid-cycle
lies above TAMS, so it is implicitly live. Wave 4's `concmark4` hunt re-checked this
argument ("SATB entries during a young cycle inside Phase 2").

## Fix (S, two files, one commit)

1. `satb_barrier`: after the `Value::Object(Some(r))` match, return unless `r` lies in the
   old generation. The card table spans exactly the old generation's fixed storage, so the
   range test is exact and one comparison pair: `self.card_table`'s covered range, or
   `is_old_gen_addr`, whose cost should be checked on the hot path.
2. `helpers.rs::jit_putfield_object_satb_pre_barrier_enqueues_old_ref`: store an OLD
   old-value, or invert its young-value assertion. The two must land together, or `vm`'s
   test run goes red.

The JIT's own pre-barrier helper routes through the same `satb_barrier`, so it needs no
separate change. Check that no other emitter logs directly.

## Owners

`satb_barrier` in `gen_heap.rs` is not in lane `concmark5`'s function set. `helpers.rs`
belongs to the JIT helper owner. This is a cross-lane request in
`docs/internal/reviews/gengc-round4-w5-concmark5-20260924.md`.

## How to verify

* `cargo test -p cratonvm-gc --lib concurrent_mark`: `satb_prevents_lost_object` and the
  Phase-2 drain tests are the correctness fence.
* `cargo test -p cratonvm-vm --lib jit_putfield_object_satb_pre_barrier` (after the test
  change).
* A count, which needs no timing:
  * `concpol_satb_drained_in_mark` on `GenR4W4SteadyPromotionProbe` at `-Xmx512m`, before
    and after the change. It should fall by the young share of reference overwrites.
  * The program output must match.

---

## 2026-09-24 round 4 wave 6 (lane `concsvc6`): the count landed; the barrier diff, exactly. STAYS OPEN (cross-lane)

**Unbuilt when written.** Lane `concsvc6` owns `concurrent_mark.rs` and the policy
sections of `gen_heap.rs`, but not `satb_barrier`, not the JIT helper test, and not
`satb.rs`, which belongs to gc-common. So only the measurement landed. The barrier change
is below as an exact diff, for the owner of `GenerationalHeap::satb_barrier` together with
the owner of `vm/src/jit/helpers.rs`, **in one commit**.

### What landed: the count the filter would remove

Phase 2's SATB drain (`ConcurrentMarker::concurrent_mark_budget`) now counts the entries
that lie outside the old generation (`OldGen::contains`). Every one of them is an entry
`markable_old_object` rejects. The count is on the shutdown line as
`concdrv_satb_outside_old_gen`, next to `concdrv_satb_drained_in_mark`. Their ratio is the
barrier volume the filter saves, before anyone changes the barrier. Remark's own drain is
not counted: that function borders lane `oldpin6`'s remark-to-sweep code, and remark sees
only the tail of the log. Test (unrun):
`concurrent_mark::tests::phase_two_counts_satb_entries_outside_the_old_generation`.

Measure first:

```
CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m \
    -cp tools/bench GenR4W4SteadyPromotionProbe 4 2>&1 | grep conc_driver:
```

Read `concdrv_satb_outside_old_gen / concdrv_satb_drained_in_mark`. Near 0 means the
filter buys nothing. A large share, the page's hypothesis, means land the diff.

### The diff (proposed; not applied)

`gc/src/gen_heap.rs`, `GenerationalHeap::satb_barrier`, after the `old_ref` match:

```diff
         let old_ref = match old_value {
             Value::Object(Some(r)) => r,
             _ => return,
         };
 
+        // gengc-r4w5-concmark5-satb-barrier-logs-young-old-values: the
+        // generational concurrent marker traces the OLD generation only, and
+        // every consumer drops a non-old entry (`markable_old_object`). A young
+        // old-value needs no logging: an old object reachable at the snapshot
+        // only through a young one is an initial-mark root
+        // (`collect_young_to_old_roots`), and a young value promoted mid-cycle
+        // lies above TAMS. The card table spans exactly the old generation's
+        // reservation (`write_barrier`'s own range test), so this is one
+        // subtraction and one compare on two already-hot fields.
+        let addr = old_ref.as_ptr() as usize;
+        if addr.wrapping_sub(self.card_table.base_addr()) >= self.card_table.region_size() {
+            return;
+        }
+
         // Log the old reference to the per-thread SATB buffer. The buffer
```

`gc/src/gen_heap.rs`, test `w5e_one_satb_gate::the_generational_barrier_gates_on_the_queue_not_the_phase`.
Its `old` is YOUNG today, so the second assertion would fail:

```diff
-            let old = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
+            // An OLD value: the barrier logs only those.
+            let old = heap
+                .try_alloc_object_old(cratonvm_types::ClassId::new(0), 1)
+                .expect("room in the old generation");
             let addr = old.as_ptr() as usize;
```

Then add a young arm at the end of the same test, which pins the filter:

```diff
+            // A YOUNG old-value is never logged, even with the queue open.
+            let young = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
+            assert!(!heap.is_in_old(young.as_ptr()));
+            q.activate();
+            heap.satb_barrier(Value::Object(Some(young)));
+            assert!(logged(&q).is_empty(), "a young old-value is not logged");
+            q.deactivate_and_discard();
             state.set_phase(ConcurrentGcPhase::Idle);
```

`vm/src/jit/helpers.rs`, test `jit_putfield_object_satb_pre_barrier_enqueues_old_ref`. Its
`old_obj` is YOUNG today:

```diff
-        let old_obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 0);
+        // An OLD old-value: the generational barrier logs only those
+        // (gengc-r4w5-concmark5-satb-barrier-logs-young-old-values).
+        let old_obj = vm_box
+            .mem
+            .heap
+            .try_alloc_object_old(ClassId::new(0), 0)
+            .expect("room in the old generation");
```

### Checked

* **Every emitter goes through `satb_barrier`.** On Generational, the only callers of
  `satb_thread_local_log` are `GenerationalHeap::satb_barrier` and a
  `concurrent_mark.rs` test. The interpreter (`interpreter.rs`, `opcodes.rs`), the JIT
  helpers (`helpers.rs`, including the compiled pre-barrier's slow path) and JNI
  (`jni.rs`) all call `heap.satb_barrier`. G1 has its own barrier (`g1.rs`,
  `satb_pre_barrier_required`), which is untouched. No emitter logs directly, so the
  one-function change covers the interpreter, the JIT and JNI.
* **The range is exact.** `write_barrier` already uses
  `card_table.base_addr()..+region_size()` as "the old-gen arena". `satb.rs` needs no
  change, and gc-common owns nothing in this diff.
* **Soundness.** This is the page's argument. Wave 4's `concmark4` hunt re-checked it for
  entries logged during a young cycle inside Phase 2.

### Retire when

The diff and both test changes land in one commit, `cargo test -p cratonvm-gc --lib
concurrent_mark` and `cargo test -p cratonvm-vm --lib
jit_putfield_object_satb_pre_barrier` pass, and the probe above prints
`concdrv_satb_outside_old_gen=0` with the same stdout as before.
