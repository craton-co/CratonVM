# Proposal: allocate capturing-lambda proxies from the TLAB

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 24
> of 54).** Not built (`invokedynamic.rs::allocate_lambda_proxy_from_values`
> still allocates on the shared path). **Gate:** a capturing-lambda
> microbenchmark, 1 and N threads, interleaved; proxy field readers checked
> against the planned shape. **Size:** S-M.

**Status: PROPOSAL** (filed 2026-09-24, gc-common round wave 9, lane C9).
Performance, all three collectors. Not a work item until triaged.

## What is there

Every evaluation of a CAPTURING lambda allocates a proxy object in
`vm/src/runtime/invokedynamic.rs`, `allocate_lambda_proxy_from_values`
(reached from the interpreter's `invokedynamic` and from the JIT's indy
bridge fast path). Since gc-common w9-c it allocates through
`interpreter::alloc_object_shared` (before that, a private copy of the old
ladder with the same first attempt). Its first attempt is
`VmHeap::try_alloc_object`: the SHARED path, which takes the backend's
allocation lock, not the thread's TLAB bump that `new` uses
(`gc_and_alloc.rs::gc_alloc_object`: `plan_tlab_object_shape_at` +
`tlab_alloc_object`, falling back to `alloc_object_shared`).

So `list.forEach(x -> sum.add(x))`, a stream pipeline's lambdas, or a
`Comparator.comparing(f)` in a hot loop take a lock per evaluation, and on
a multi-threaded workload the lambdas of all threads contend on it, while
an equally sized `new` does not. Since w9-c each shared-path proxy also does
an atomic add on `bytes_allocated_total` (correct accounting, which it used
to skip), which a TLAB allocation batches per thread.

## Proposed direction

Give the proxy `new`'s front end: plan the shape with
`plan_tlab_object_shape_at(proxy_class_id, num_captures, <a lambda site>)`,
try `tlab_alloc_object`, fall back to `alloc_object_shared`. Points to
settle first:

- the proxy's class id is synthetic (`>= 0x8000_0000`, absent from the
  class store), so the shape planner must answer the LEGACY body the shared
  path gives today, or every reader of proxy fields (the interpreter's
  lambda dispatch, the JIT's inline field loads, `lambda_proxy` natives)
  must be checked against a compact body;
- a TLAB object is young; the proxy's capture stores need the same barrier
  story as any `new` followed by `putfield`;
- measure: an interleaved A/B of a capturing-lambda microbenchmark with 1
  and N threads (the in-JVM timings here swing ~3x between reps; take
  medians).

## Confirmation

```bash
rg -n "fn allocate_lambda_proxy_from_values" -A120 vm/src/runtime/invokedynamic.rs | rg -n "alloc_object_shared|tlab_alloc_object"
```

## What would retire it

Proxies allocated from the TLAB with an unchanged object shape, and a
measured multi-threaded improvement (or a measured null result recorded
here).
