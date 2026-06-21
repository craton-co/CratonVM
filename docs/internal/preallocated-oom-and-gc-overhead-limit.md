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

## Layer 4 — collector promotion abort — FIXED (merge `0f4177c4`)

The moving (Cheney) young collector aborts the **process** mid-collection when
it cannot relocate a survivor — old gen full AND young to-space overflowed during
promotion-fallback (`gen_heap.rs` ~5255). This fires *inside* `collect_garbage`,
before the overhead limit (built from completed GCs) can.

**Fix (`collect_garbage_inner`):** also route to the **non-moving young sweep**
(`sweep_young_non_moving` — already the default + superior collector while JIT
frames are active) when old gen cannot absorb a full young's worth of survivors
(`old_gen_free < young_from_used`, the necessary precondition for the abort). The
non-moving sweep never relocates, so it can't hit the abort: it reclaims dead
young in place and leaves un-promotable survivors in young (graceful
`old_full = true`, line ~3791), after which the allocation paths / GC-overhead
limit surface a *catchable* OOM (the singleton). Opt out:
`CRATONVM_NO_GC_PROMOTION_GUARD`. (The chosen approach is a refinement of the
"non-moving fallback"; the pre-GC capacity-gate variant `young_from_used >
to-space + old_free` can never fire because to-space == from-space size.)

Validated == HotSpot (caught + "alive after OOM"): `ObjAllocOom.objLoop` at **16m
JIT-off, 64m JIT-on, 64m JIT-off** (3–7s), where it previously aborted. No
regression: bintrees10/14/18 = 135854/3222190/68332206 (reroute doesn't fire at
8g), sieve250k=22044, IrCall inc-22, BigArrayOom caught.

## "Layer 5" was a MEASUREMENT ARTIFACT — not a real issue

An earlier write-up of this doc claimed `ObjAllocOom.objLoop` at **16m + JIT-on**
caught "too slowly (~70s)". That was **CPU contention from concurrent `cargo`
builds**, not a VM pathology: every slow reading was taken while a release build
saturated the machine (bt18 also read 60–76 s in those windows vs its ~23–35 s
normal). Measured cleanly (no build running), objLoop @16m JIT-on completes in
**1–2 s** (caught + "alive after OOM", == HotSpot), and `CRATONVM_DBG_DEOPT`
reports **0 deopts** — refuting both the "slow non-moving GC" and the
"deopt-thrash" theories. **All four configs** (16m/64m × JIT on/off) catch fast.
The heap-full catchable-OOM work is complete.

Lesson: do not trust VM wall-clock or GC-frequency numbers gathered while a build
(or any CPU hog) is running — they inflate 3–14×. (The layer-4 collector *abort*
fix above is real and load-independent — a deterministic `process::abort()` — and
stands.)

## Note

The earlier `POST-GC STALE STACK … objLoop` warnings seen during exploration did
NOT reproduce on the diagnostic runs (`stale=0`); they appear tied to a different
(JIT-frame) scenario and are not an `objLoop` blocker.
