# G1: a latched GC-overhead streak threw `OutOfMemoryError` without a marking cycle

> **STATUS (2026-09-29, gc defects round, orchestrator, wave d10 verification on the Linux release build `cratonvm-gcd-d10` (`b30b8abaa`) against wave d9's `cratonvm-gcd-d9`, same load, two runs per row per collector (`docs/internal/gc-defects-round-20260927/verify-d10/vd10.list`)): fixed in code, NOT VERIFIED on G1.** None of the G1 OOME rows gets far enough to reach a latched streak: `GenR4W4HeapFullThrashProbe`, `FullHeapResolveProbe`, `GenR4W4NativeStringOomProbe 4096` and `NativeGrowthReclaimProbe` all exit before their first output line with an uncaught `OutOfMemoryError`, on d9 and d10 alike (`gcd-d10v-g1-heap-filling-probes-exit-before-any-output-with-an-uncaught-oome-20260929.md`). `GenR4W6JitOomRootProbe -Xmx64m` timed out (rc 124) on G1 in both d10 runs and in one of d9's. Retire this page once that start-up failure is fixed and a G1 thrash run shows a marking cycle before the latched error.

*Filed 2026-09-28 by gcd d10/o (lane oom10), by reading the allocation doors
per collector. Half fixed in-session (the interpreter's and the native
factories' doors); the JIT helpers' and the exception door's halves are
edits in lane f's files, given exactly below.*

> **STATUS (2026-09-28, gcd d10/f third commit, lane frames10): FIXED IN
> CODE on every door, unbuilt; default on, `=0` switch
> `CRATONVM_GC_OVERHEAD_PROGRESS=0`.** Lane f applied the two remaining
> edits below after reviewing them as the owner of those files. The only
> change from the text below: each G1 arm samples `live_bytes_estimate` only
> on G1 (inside the G1 branch, after a soft rung that collected nothing, so
> the value is the same), which keeps every other collector's path exactly
> as it was.
> - `vm/src/jit/helpers.rs`: `jit_latched_overhead_limit_throws` got the G1
>   arm, and `jit_g1_overhead_limit_cycle` is new.
> - `vm/src/runtime/exceptions.rs`: `create_exception_object_for_class_inner`,
>   latched arm.
> - `vm/src/vm/vm_exec.rs`: `NativeContextImpl::reclaim_before_alloc_retry`
>   now calls `native_reclaim_before_alloc_retry` (d10-o report section 4,
>   item 1).
>
> Owner's review notes:
> - The cycle runs where the existing Generational major already runs, with
>   no object reference held across it (the helpers' contract; the
>   exception object is not yet allocated).
> - A futile cycle keeps the verdict, so a heap wedged on live data still
>   throws.
> - A productive cycle resets the streak, and the JIT helper then retries.
>   If that retry fails, the helper's own `jit_g1_last_ditch_full_cycle` may
>   run a second cycle before the OOME, the same rung the unlatched path
>   has.
>
> Retire when the G1 run under "How to verify" passes 3/3 and
> `cargo test -j 5 -p cratonvm-vm --lib gcd_d10o_ladder_tests` passes.

- **Status:** (d10/o's, superseded above) PARTLY FIXED (default on, `=0` switch
  `CRATONVM_GC_OVERHEAD_PROGRESS=0`); open in `vm/src/jit/helpers.rs` and
  `vm/src/runtime/exceptions.rs`.
- **Severity:** medium on G1 -- a spurious `OutOfMemoryError` after a
  program dropped its data: the heap is reclaimable, but only by a
  collection no rung ran. Generational and ZGC are not affected.
- **Collectors:** G1 only (the shape needs a forced collection that cannot
  reach dead Old regions).

## What was wrong

With the overhead streak latched (`gc_overhead_limit_exceeded`), every
allocation ladder honoured the verdict after the forced collection and the
soft-reference rung:

| door | Generational | ZGC | G1 |
|---|---|---|---|
| interpreter `collect_and_retry_with_thread` | major(s) first (gen r4w4, gcd d4/j) | forced cycle is whole-heap | one attempt, then the error ("G1's last-ditch cycle stays off this fast-fail exit") |
| native `reclaim_before_alloc_retry` | soft rung's answer only | soft rung's answer only | soft rung's answer only |
| JIT `jit_latched_overhead_limit_throws` | majors (gcd d1/b, d5/u) | throw unless the soft rung collected | throw unless the soft rung collected |
| exceptions `create_exception_object_for_class_inner` | majors (gcd d2/j) | one attempt | one attempt |

On G1 the forced collection is a YOUNG pause; dead Old and humongous
regions are reclaimed only by a completed marking cycle's cleanup
(`g1_force_full_cycle`). The G1 streak latches when the heap has under 2 %
headroom (`old_gen_headroom` is whole-heap headroom on G1) and forced pauses
free under 2 %: exactly a program that filled the heap, caught the
`OutOfMemoryError` and dropped its data. Its next allocation met the latched
verdict with the dropped data still in Old regions, and a program holding no
live `SoftReference` got no collection that could see it. HotSpot's G1 runs
a full collection before every heap `OutOfMemoryError`.

## Landed (gcd d10/o, `vm/src/runtime/interpreter/gc_and_alloc.rs`)

- `g1_overhead_limit_full_cycle(shared, thread, before_live)`: on G1 (and
  with `CRATONVM_GC_OVERHEAD_PROGRESS` on), `last_ditch_reclaim`'s marking
  cycle (`g1_last_ditch_full_cycle`, split out of `last_ditch_reclaim`,
  soft rule armed when soft references are live); `true` when it freed at
  least 2 % of the heap since `before_live`, and then the streak is reset.
  The Generational exit's rule (gen r4w5/thrash5): a futile full collection
  keeps the verdict, so a G1 heap wedged on live data still throws rather
  than limping on one cycle per object. `CRATONVM_DBG_GC_OVERHEAD=1` prints
  `[GC_OVERHEAD] g1-overhead-cycle: ... productive=`.
- `collect_and_retry_with_thread`, the unjudged `Attempt` arm: on G1 a
  failed attempt runs that cycle and attempts once more if it was
  productive. Every other backend and G1 under `=0` keep the one attempt.
- `native_reclaim_before_alloc_retry` (the native factories' ladder, live
  once lane f applies the `reclaim_before_alloc_retry` request in
  `docs/internal/gc-defects-round-20260927/d10-o-report.md`): the same G1 arm.

## Remaining edits (lane f)

1. `vm/src/jit/helpers.rs`, `jit_latched_overhead_limit_throws`:
   ```diff
    fn jit_latched_overhead_limit_throws(vm: &SharedVm) -> bool {
   +    // gcd d10/o: sampled before the soft rung, for the G1 verdict below.
   +    let before_live = vm.mem.heap.live_bytes_estimate();
        let soft_collected = jit_overhead_limit_clear_soft_refs(vm);
   +    // gcd d10/o: on G1 no collection so far reached a dead Old region, and
   +    // one marking cycle decides (`g1_overhead_limit_full_cycle`): productive,
   +    // the helper's ordinary retry; futile, or declined under
   +    // `CRATONVM_GC_OVERHEAD_PROGRESS=0`, the error as before.
   +    if !soft_collected && vm.mem.heap.is_g1() {
   +        return !jit_g1_overhead_limit_cycle(vm, before_live);
   +    }
        let judge = vm.mem.heap.is_generational() && cratonvm_types::flags().gc.gc_overhead_progress;
   ```
   and, next to `jit_overhead_limit_major`:
   ```rust
   /// gcd d10/o: the G1 arm of [`jit_latched_overhead_limit_throws`].
   #[cold]
   fn jit_g1_overhead_limit_cycle(vm: &SharedVm, before_live: usize) -> bool {
       // SAFETY: the `jit_overhead_limit_major` contract -- called only from
       // the JIT allocation slow-path helpers, on the mutator thread that
       // entered compiled code, with no other `JitThreadGuard` of it live.
       let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
           return false;
       };
       crate::runtime::interpreter::gc_and_alloc::g1_overhead_limit_full_cycle(
           vm,
           thread,
           before_live,
       )
   }
   ```
   (`jit_overhead_limit_verdict` and its tests are untouched: the G1 arm
   returns before it.)
2. `vm/src/runtime/exceptions.rs`, the latched arm of
   `create_exception_object_for_class_inner` (after the Generational
   `majors_to_decide_oome` block):
   ```diff
   -                match shared.mem.heap.try_alloc_object_full(class_id, num_fields) {
   +                let mut retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
   +                // gcd d10/o: on G1 the latched arm ran no marking cycle; one
   +                // decides (declines off G1 and under
   +                // `CRATONVM_GC_OVERHEAD_PROGRESS=0`).
   +                if retried.is_none() && !soft_collected {
   +                    let before_live = shared.mem.heap.live_bytes_estimate();
   +                    if super::interpreter::gc_and_alloc::g1_overhead_limit_full_cycle(
   +                        shared,
   +                        thread,
   +                        before_live,
   +                    ) {
   +                        retried = shared.mem.heap.try_alloc_object_full(class_id, num_fields);
   +                    }
   +                }
   +                match retried {
                        Some(obj) => obj,
   ```

## How to verify

Unit: `cargo test -j 5 -p cratonvm-vm --lib gcd_d10o_ladder_tests`.

The shape needs a G1 heap wedged to under 2 % headroom and then dropped.
`GenR4W4NativeStringOomProbe` is that shape (fill to the wall, catch, drop
everything, allocate); on G1 it needs the 4 MiB sliver:

```
for i in 1 2 3; do
  CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm -XX:+UseG1GC -Xmx64m -cp tools/bench \
      GenR4W4NativeStringOomProbe 4096 2>g1ns.$i.err; echo rc=$?
  grep -c 'g1-overhead-cycle' g1ns.$i.err
  CRATONVM_GC_OVERHEAD_PROGRESS=0 timeout 300 cratonvm -XX:+UseG1GC -Xmx64m -cp tools/bench \
      GenR4W4NativeStringOomProbe 4096; echo "[=0] rc=$?"
done
```

Expected: HotSpot G1's four lines (`fill: OutOfMemoryError "Java heap
space"`, `native-strings ok`, `recovered ok`, `PASS`) and rc 0; a
`g1-overhead-cycle` line with `productive=true` whenever the streak had
latched (zero such lines means the streak never latched on this run, and the
run says nothing about this page). The G1 humongous refusal
(`g1-humongous-refusal-with-free-space-and-old-array-fixup-misses.md`) can
still fail the run before the ladder is reached; a failure with no
`g1-overhead-cycle` line is that page's. Retire this page when the JIT and
exceptions halves have landed and the G1 run passes 3/3.
