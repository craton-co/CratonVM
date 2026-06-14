# Group 02 — Native alloc young-gen OOM hard-abort  (FIXED)

**Status:** FIXED, merged to `dev` (`b7525126`, commit `f5727c3b`).
**Affected:** every embedded-server class at the default heap (deployment).
**HotSpot:** PASS (default heap is RAM-proportional, multi-GB).

## Symptom

During `ContextConfig` (examples-webapp deploy), the VM died with:

```
FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 104 bytes,
from-space has 67108800/67108864 used
```

`std::process::abort()` — not a catchable Java OOM. Initially looked like an
infinite loop / hang; bisection (JIT off reproduced identically; `-Xmx2g`
removed it; memory steady ~520MB) showed it was a memory wall, not a loop.

## Root cause

CratonVM sizes its young semi-space at `Xmx/4`; the default `Xmx` (~256MB →
64MB young) is far smaller than HotSpot's default. A webapp-deploy working set
can't fit, so the convenience native allocators — `ctx.new_object` /
`new_array` / `new_ref_array` / `alloc_concurrent_synthetic`, which funnel into
the **panicking, non-GC-retrying** `GenerationalHeap::alloc_object`/`alloc_array`
→ `alloc_young` — aborted the process the instant young from-space filled. A
single native bulk-allocation past young could thus hard-kill the VM at ANY heap
size.

The interpreter's `gc_alloc_*` path GC-and-retries, but the native path cannot:
native methods hold raw `ObjectRef`s in Rust locals that are in NO GC root set,
so triggering a moving/promoting young GC from there would relocate those
objects and dangle the locals (the stale-ref SEGV class).

## Fix (`gc/src/gen_heap.rs`)

On young-full, the panicking allocators now spill the single allocation directly
into the **old generation** (new `try_alloc_object_old`, mirroring the existing
`try_alloc_array_humongous`; `GC_FLAG_OLD_GEN` so minor GC won't forward it).
Old-gen allocation relocates nothing, so every native-held ref stays valid and
no GC runs. Falls through to the original abort only if old gen is also full.

## Validation

- bt10/14/16/18 checksums unchanged (bt18 = 68332206 = HotSpot); no GC-path
  regression (the panicking allocator is not the hot interpreter/JIT path).
- TestSsl at the **default** heap no longer aborts — it proceeds through
  ContextConfig via old-gen spill + heap expansion; no stale-ref SEGV over
  850s+ of native-heavy load.

Two follow-ups also landed: the harness now passes `-Xmx2g` (more headroom), and
the deployment-slowness investigation is group 03/04.

## Reproduction (pre-fix)

Run any embedded-server class (e.g. `TestSsl`) at the **default** heap (no
`-Xmx`) and it abort-OOMs in `ContextConfig`.
