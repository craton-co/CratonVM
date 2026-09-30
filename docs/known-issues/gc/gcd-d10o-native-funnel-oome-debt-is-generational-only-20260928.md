# The native funnel's "collect after an `OutOfMemoryError`" debt is Generational-only

*Filed 2026-09-28 by gcd d10/o (lane oom10), by reading. Not reproduced on
G1 or ZGC; filed rather than fixed because the paying call sits in lane j's
region (`safe_native_call_impl`) and a half-change misbehaves on G1 (below).*

- **Status:** OPEN.
- **Severity:** medium on G1 and ZGC -- a spurious `OutOfMemoryError` from a
  single-attempt native allocation after the program caught an
  `OutOfMemoryError` and dropped its data. Generational is fixed (gcd d3/o,
  d4/j, d5/q).
- **Collectors:** G1, ZGC.

## What is wrong

A native's own allocations (`create_string`, `new_array`, `alloc_object`,
...) get ONE attempt and never collect (`runtime/native_oom.rs`); what
collects for them is the funnel, `vm_exec::safe_native_call_impl`, before
the callback: its young-pressure relief (skipped while the overhead streak
is latched) and, since gcd d4/j, the OOME debt -- `note_heap_oome_raised`
arms a per-VM debt word when a heap `OutOfMemoryError` is raised, and the
funnel pays it with `majors_to_decide_oome` on the raising thread's next
native call (`native_call_owes_oome_major`). That repaired
`gcd-d3o-native-door-first-allocation-throws-without-a-collection` shapes A
and B: after `made = null; fill = null`, the first native to allocate met the
pre-drop heap and threw.

Both halves are Generational-only in
`vm/src/runtime/interpreter/gc_and_alloc.rs`: `note_heap_oome_raised` arms
only `if shared.mem.heap.is_generational()`, and `native_call_owes_oome_major`
clears the word off Generational. On G1 and ZGC the shape stays open: a
latched streak skips the relief, nothing else collects, and a native that
allocates first after the drop (`String.valueOf`, `Integer.toString`,
`StackTraceElement` materialisation, a reflection result) throws with the
heap reclaimable. ZGC's forced cycle is whole-heap and would free it; G1's
young pause would not reach dead Old regions.

## Why not simply widen the arming

The funnel pays with `majors_to_decide_oome`, which on G1 runs up to two
YOUNG pauses (the major request is Generational's) and frees nothing held
in Old regions -- a cost with no effect. The payment has to dispatch per
backend, and the paying line is lane j's.

## Proposed fix

1. `gc_and_alloc.rs` (lane o): arm the debt on every backend
   (`note_heap_oome_raised`: drop `shared.mem.heap.is_generational() &&`;
   `native_call_owes_oome_major`: drop the `!is_generational()` clause of
   its defensive check), and add the payment:
   ```rust
   /// The funnel's payment of the OOME debt: the collector's full collection.
   pub fn pay_native_oome_debt(shared: &SharedVm, thread: &mut JvmThread) {
       if shared.mem.heap.is_generational() {
           let _ = majors_to_decide_oome(shared, thread, "native-after-oome-major");
       } else if shared.mem.heap.is_g1() {
           g1_last_ditch_full_cycle(shared, thread);
       } else {
           let _ = maybe_gc_forced_collected(shared, thread, "native-after-oome-cycle");
       }
   }
   ```
2. `vm_exec.rs::safe_native_call_impl` (lane j): replace the
   `majors_to_decide_oome(shared, thread, "native-after-oome-major")` call
   under `native_call_owes_oome_major` with
   `crate::runtime::interpreter::pay_native_oome_debt(shared, thread)`.

Both in one change (1 without 2 is the G1 cost above). `recovered` stays
`old_gen_headroom() >= cap / 8`, whole-heap headroom on G1 and ZGC. Keep
`CRATONVM_GC_OVERHEAD_PROGRESS=0` as the switch (it already disarms the
debt).

## How to verify

```
for gc in UseG1GC UseZGC; do for i in 1 2 3; do
  CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm -XX:+$gc -Xmx64m -cp tools/bench \
      GenR4W4NativeStringOomProbe 4096 2>ns.$gc.$i.err; echo "$gc rc=$?"
  grep -c 'native-oome-debt' ns.$gc.$i.err
done; done
```

Expected after the fix: HotSpot's four lines (G1 / ZGC with the 4 MiB
sliver, measured on Temurin 25.0.3), and `[GC_OVERHEAD] native-oome-debt:
... pay=true` lines whenever a native was the first allocation after the
drop. Before the fix: no `native-oome-debt` line at all on G1 / ZGC.
