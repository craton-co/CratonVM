# The compact TLAB allocation shape, and a miscompile that now reproduces on demand

*2026-09-03. Branch `perf/compact-tlab-alloc-20260903`.*

## What this is for

Three fast paths in a row turned out to be dead because the object they were
gated on was not compact: the IR tier's inline `getfield` (fixed 2026-08-18),
the optimizing tier's gated reference store, and the single-pass one (both
2026-09-02). Each was fixed by emitting a second, legacy-shaped arm. The
common cause was never touched:

`init_object_header` — the interpreter's TLAB fast path — writes a LEGACY
header unconditionally, whatever compact layout the class has registered,
because it never consults `plan_object_alloc`.

It is worth being precise about who does what, because the earlier pages in
this series were not precise enough:

| allocator | shape |
|---|---|
| JIT inline `new` (`emit_inline_tlab_new`) | **compact**, with a layout-replace guard |
| `gen_heap::alloc_object` (TLAB miss, large objects) | **compact** |
| interpreter TLAB fast path | **legacy** |
| `jit_new_object` helper → TLAB | **legacy** (it routes through the above) |

So the same class already gets both shapes today depending on which allocator
ran, and every field access keys on the per-object `GC_FLAG_COMPACT` bit
precisely because of that. What is missing is not correctness but consistency:
the hottest allocation path is the one producing the shape every fast path
then has to have a second arm for.

## What this change does

`plan_tlab_object_shape` makes ONE shape decision per allocation and carries it
to the header stamp, so the reserved size and the stamped header cannot come
from two lookups that might disagree (the hazard the JIT's inline emitter
carries a layout-replace guard for). Every TLAB object site now uses it — the
interpreter's `gc_alloc_object`, both JIT-helper sites, the native-call site in
`vm_exec`, and the compact-String site in `vm_object` — which is what the
existing comment in `tlab_alloc_object_inner` demanded: *"If the two shapes are
ever unified it has to be done at every allocation site at once."*

The predicate is the same one the JIT's inline `new` uses — a registered layout
whose `field_count` matches this allocation exactly — plus a refusal unless the
process has a single `ClassStore`, since the interpreter's fast path has no
cheap route to the owning heap's layout domain and that is exactly the
condition under which the domain screen is vacuous.

A census comes with it, because a shape change with no count is unreadable:

```
[cratonvm] TLAB object shapes: compact=8538 legacy=6560 bytes-saved=210656
```

## It is default OFF, and here is why

`CRATONVM_COMPACT_TLAB_ALLOC=1`. The switch exists because turning it on
**miscompiles**, and now does so deterministically:

| collector | `FjpProbe` sum, switch off | switch on |
|---|---|---|
| Generational | 499999500000 `OK` | **215812748544** (no `OK`) |
| G1 | 499999500000 `OK` | **215812748544** (no `OK`) |
| ZGC | 499999500000 `OK` | 499999500000 `OK` |

HotSpot's answer is 499999500000. This is the same probe the comment in
`tlab_alloc_object_inner` names — *"a 2026-09-02 attempt … MISCOMPILED
`probes/FjpProbe.java`: wrong per-task sums, no collection involved"* — except
that comment attributes the attempt to the ZGC arm, and ZGC is the one
collector that now passes.

That turns a one-off anecdote into a lever anyone can pull, which is the actual
deliverable here.

## What the reproduction already rules out

Three experiments, each a single run:

1. **It is not in the JIT.** `CRATONVM_NO_JIT=1` with the switch on still
   produces a wrong sum (217283218880). Every compiled-code emitter — the
   gated reference stores, the inline `getfield`, the inline `new` — is
   therefore off the list.
2. **It is not the atomic intrinsics.** `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1`,
   `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC=1`, and both together all reproduce
   it unchanged — worth checking because `AtomicInt`/`AtomicLongFieldLayout`
   are the two places that bake a compact AND a legacy address and pick per
   object.
3. **It is collector-dependent, and the difference is object registration.**
   ZGC passes; Generational and G1 fail. The one structural difference on this
   path is `note_tlab_object`: ZGC records the TLAB object and its footprint in
   its object-start registry, while `VmHeap::Generational | VmHeap::G1` discard
   both arguments. Those two collectors therefore have to rediscover a TLAB
   object's size by walking and reading its header — which is exactly the thing
   this change alters.

That is a specific, checkable lead and it is where the next session should
start. It is **not** a diagnosis, and this page does not claim one.

## Status

- Default OFF; `CRATONVM_COMPACT_TLAB_ALLOC=1` reproduces the defect on
  Generational and G1 in one run.
- With the switch off, every path is byte-for-byte the behaviour it had before:
  `plan_tlab_object_shape` returns the same `HEADER_SIZE + num_fields *
  SLOT_SIZE` those sites computed inline, and a `debug_assert` in each TLAB
  wrapper pins that the reserved size is the one the shape plan asked for.
- Regression suite 88/88 on the default collector and 88/88 under
  `-XX:+UseGenerationalGC` with the switch off.

## The prize, for whoever fixes it

On `FjpProbe` alone the switch converts 8,538 of 15,098 TLAB objects to the
compact shape and saves 210,656 bytes — ~14 KB per thousand objects, on a
probe that is not allocation-heavy by this repo's standards. `header-shrink.md`
puts `HashMap.Node` at 72 bytes compact against 96 legacy. And the three
second-arm fast paths this series added exist only because the common case is
the shape this switch would retire.
