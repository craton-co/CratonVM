# Proposal: HotSpot's preallocated `OutOfMemoryError`s, one per message, with a pool that carries stack traces

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 52
> of 54).** Partly built opt-in:
> `vm/src/vm/realms/heap_realm.rs::PreallocatedOome`, `OomeKind { ArraySize,
> Metaspace }` under `CRATONVM_GC_PREALLOCATED_OOME_KINDS` (default off). No
> d7 row ran the flag. Left: the `GcOverhead` kind, the trace-carrying pool,
> dropping the VM-limit collection. **Gate:** flip the landed kinds first:
> `GenR4W5OomHooksProbe`, `GenR4W6MirrorOomProbe` and the thrash rows print
> the default arm's lines with the flag (d7 default: oom_hooks prints `first:
> ... "Java heap space"`, `second: ... "Requested array size exceeds VM
> limit"`). **Size:** XS (flip), S (rest).

*Filed 2026-09-26 by gen round 5 wave 1, lane `oom5`. A direction, not a
defect; the defect it would close is item 4 of
`../../internal/gc/gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928.md`.*

## Today

One preallocated throwable, `SharedVm::singleton_oom` ("Java heap space"),
created by `runtime::exceptions::ensure_singleton_oom` before `main`, rooted
by `memory/roots.rs` step 8c and remapped by `memory/gc.rs` step 6c. Every
VM-raised `OutOfMemoryError` that cannot build a fresh throwable falls back
to it, whatever its message; and because it is one object, a program that
calls `addSuppressed` on a caught OOME mutates every later one.

## HotSpot

`Universe::genesis` preallocates one instance per message (Java heap space,
Metaspace, Compressed class space, Requested array size exceeds VM limit, GC
overhead limit exceeded, "Java heap space: failed reallocation of scalar
replaced objects", "Java heap space: failed retryable allocation") plus a
pool of `PreallocatedOutOfMemoryErrorCount` (default 4) instances.
`Universe::gen_out_of_memory_error` copies a pooled instance, sets the
message's default's detail and fills in a fresh stack trace while the pool
lasts, and falls back to the message's default instance (no trace) when it
is empty.

## Proposed shape

1. `heap_realm`: `preallocated_oome: RwLock<PreallocatedOome>` with one slot
   per message kind (an enum `OomeKind { JavaHeap, ArraySize, GcOverhead,
   Metaspace }`) and a small pool vector. Per VM, not a process global.
2. `ensure_singleton_oom` becomes `ensure_preallocated_oome`: builds every
   default and the pool on the empty heap.
3. ONE root section and ONE remap step iterate the whole struct (replacing
   8c/6c), so adding a kind is a data change, not a new root.
4. `throw_runtime_error` / `jit_alloc_oom` / `osr_drain_oom_singleton` ask
   `preallocated_oome_for(message)`: a pooled instance with a fresh
   VM-side trace while one is left, the kind's default otherwise.
5. Drop the VM-limit collection this wave added in `create_vm_oome_object`
   once the array-size default exists (HotSpot collects nothing for it).

## Cost and risk

A few hundred bytes of preallocated old-gen objects per VM, created before
`main` on an empty heap. The root/remap change touches `memory/roots.rs` and
`memory/gc.rs`, whose sections other lanes own; landing it wants those lanes
or the orchestrator. Behind `CRATONVM_GC_PREALLOCATED_OOME_KINDS` until the
OOME probes (`GenR4W5OomHooksProbe`, `GenR4W6MirrorOomProbe`, the thrash
probes) print HotSpot's lines with it.
