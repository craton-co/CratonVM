# Pre-allocated singleton OOM + GC-overhead limit (catchable heap-full OOM)

**Status:** building blocks LANDED on `dev` (branch `feat/preallocated-oom`,
merge `a54f006a`). The headline goal — a fully-retained heap (`ObjAllocOom.objLoop`)
throwing a *catchable* `OutOfMemoryError` instead of aborting — is **NOT fully
reached**: it requires a 4th, deeper core-collector fix (below). Follow-up to
[jit-object-array-alloc-oom-FIXED.md](jit-object-array-alloc-oom-FIXED.md).

## Problem

When the heap is filled ~100% by *retained* objects, CratonVM hard-aborts
instead of throwing a catchable OOM, because at that point it cannot allocate the
OOM exception object itself. HotSpot pre-allocates a singleton OOM for exactly
this case. Investigating revealed the abort is layered across **four** sites:

1. `alloc_young` — the raw young-gen allocator (fixed earlier: the JIT/interpreter
   alloc paths are fallible → catchable OOM).
2. **Exception materialization** — `create_exception_object` allocated the detail
   message `String` via the non-fallible `alloc_object` → aborted on a full heap
   (the ~104-byte String object).
3. **GC death-spiral** — once the alloc path is catchable, the OOM path's GC frees
   a sliver each cycle, letting the retained loop limp on: an O(n²) thrash with no
   GC-overhead limit.
4. **Copying-collector promotion failure** — when survival ≈ 100% and old gen is
   full, the Cheney to-space cannot hold all survivors and the collector aborts
   *mid-cycle* (`gen_heap.rs` ~5255: "GC could not relocate a live object …
   during promotion"). A copying collector has no valid address for an
   un-evacuated object, so it cannot simply skip it.

## What landed (sound, no-regression)

**Singleton OOM (layers 1–2):**
- `SharedVm::singleton_oom: RwLock<Option<ObjectRef>>`, pre-allocated once before
  user `main` via `exceptions::ensure_singleton_oom` (called from `vm-cli`), and
  rooted permanently in `memory::roots` (alongside the VarHandle roots).
- `vm_object::alloc_java_string_object_from_units` refactored into a fallible core
  `try_alloc_java_string_object_from_units` (via `try_alloc_object_full` /
  `try_alloc_array_full`) + a non-fallible abort wrapper; new public
  `try_create_java_string`.
- `create_exception_object` builds the message `String` fallibly → returns
  `Err(OutOfMemoryError)` instead of aborting. `throw_runtime_error` (interpreter
  path) and `jit_alloc_oom` (JIT path) fall back to the singleton on an OOM `Err`.

**GC-overhead limit (layer 3, HotSpot `UseGCOverheadLimit` analogue):**
- `maybe_gc_forced` records each forced GC's freed bytes (`before − after` live).
  `note_gc_productivity`: a forced GC that freed < 2% of capacity increments
  `SharedVm::gc_unproductive_streak`; a productive one resets it. The *freed
  amount* (not post-GC fullness) is the correct signal for a generational heap —
  in a death-spiral the young semi-space is emptied each cycle (so total fullness
  sits ~50% and never looks exhausted) yet net freeing is ~0 because every
  survivor is promoted into an already-full old gen.
- `gc_overhead_limit_exceeded()` is true after `GC_OVERHEAD_LIMIT_CYCLES` (=8;
  env `CRATONVM_GC_OVERHEAD_LIMIT`, `0` disables) consecutive unproductive forced
  GCs. The six alloc-failure sites — `alloc_object_shared`, `gc_alloc_array`,
  `jit_new_object`, `jit_newarray`, `jit_anewarray_object`,
  `create_exception_object` — surface OOM when it trips, instead of retrying into
  the spiral. (Diagnostic: `CRATONVM_DBG_GC_OVERHEAD=1`.)

These never regress a healthy workload: a forced GC only happens on genuine
allocation failure, and only 8 consecutive sub-2%-freeing ones trip the limit —
a state a healthy heap never reaches. Validated: bintrees10/14/18 =
135854/3222190/68332206 (no false OOM at 8g), BigArrayOom caught, NegArray
`NegativeArraySizeException`, fresh-OOM path intact.

## Known remaining (layer 4 — separate, core-collector, higher risk)

`ObjAllocOom.objLoop` (a `while(true)` retaining every `new Node()`) still
aborts — now inside the **copying collector** during promotion (layer 4), which
happens mid-`collect_garbage`, *before* the overhead limit (built from completed
GCs) can fire. Diagnostic run confirmed the streak climbs correctly (freed=0,
unproductive) but a promotion-failure abort lands at streak≈5.

Making `objLoop` fully catchable requires one of:
- **Recoverable promotion failure:** thread the "could not relocate" failure up
  out of the Cheney copy loop and abort the *collection* cleanly (revert / mark
  OOM) rather than `process::abort()` — invasive (the copy runs per live object).
- **Non-moving fallback collection** when to-space + old-gen can't hold all
  survivors.
- **Headroom reservation / pre-GC capacity gate:** refuse to start a moving GC
  that provably can't evacuate (e.g. live young > to-space + old free) and OOM
  upfront; then the overhead-limit/singleton path takes over.

All three are core-collector changes and are tracked as a separate task. The
landed building blocks (singleton + overhead limit) are prerequisites for any of
them.

## Note

The earlier `POST-GC STALE STACK … objLoop` warnings seen during exploration did
NOT reproduce at JIT-off on the diagnostic run (`stale=0`); they appear tied to a
different (JIT-frame) scenario and are not the `objLoop` blocker — layer 4 is.
