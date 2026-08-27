# kfusion's per-voxel `Short2` is gone: `volume` now costs what `rawseg` costs

## Status

**FIXED, 2026-08-27.** Both allocations `new Short2()` makes are deleted, and
the arm that reads a voxel through `VolumeShort2.get` has converged onto its own
control — the same two segment reads with no wrapper object at all. **10.9x on
the target arm**, and what is left of it is the FFM element read, not the
wrapper.

Same binary, flag A/B, `probes/VoxelAlloc2.java` at 64³ voxels, steady state:

| arm | `volume` | `rawseg` (the CONTROL) | `array` (the floor) |
|---|---:|---:|---:|
| neither flag | 478 ns/voxel | 57.0 | 5.2 |
| `CRATONVM_JIT_IR_INLINE=1` | 227 ns/voxel | 38.8 | 4.8 |
| `CRATONVM_SCALAR_DEOPT=1` | 479 ns/voxel | 57.0 | 5.1 |
| **both** | **43.9 ns/voxel** | 39.2 | 4.9 |
| *real HotSpot 25, same probe* | *1.4* | *1.4* | *0.5* |

Checksum `268171424` in every row of every arm, by construction, so a transform
that broke the read would show up as a wrong number rather than as a fast one.

**`volume` minus `rawseg` was 421 ns and is now 4.7 ns.** That difference *is*
the allocation: the two arms perform the same two `MemorySegment` element reads
and differ only in whether a `Short2` is built to carry them, so the control is
self-calibrating and the host's load cancels out of it. HotSpot's own row is the
same shape — `volume == rawseg` — which is what says the control is the right
yardstick and 4.7 ns is noise rather than a residue.

The instrument agrees with the clock:

```
[cratonvm-scalarnew] VoxelAlloc2.sweepVolume(...): scalar-replaced 1/2 alloc(s)
[cratonvm-scalarnew]   new node 61: REPLACED
[cratonvm-scalarnew]   newarray node 62: refused StoredIntoAnotherObject
[cratonvm-scalarnew] VoxelAlloc2.sweepVolume(...): scalar-replaced 1/1 alloc(s)
[cratonvm-scalarnew]   newarray node 62: REPLACED
```

Two rounds, two allocations, nothing left.

**Both flags are still opt-in**, and the table says why that matters: neither
one alone does anything much. See [What is still gated](#what-is-still-gated).

## What the page had wrong, and how the instrument said so

The page's open question was "**why** EA reports `0/2` on the merged graph is not
yet diagnosed", and it assumed one cause. There were two, and neither was the
one the page's history pointed at.

The first fix was not a fix at all — it was an instrument. `scalar-replaced 0/2`
is a count, and a count cannot be acted on; the page had already spent three
rounds guessing at that number, twice wrongly. `find_scalar_replacements` now
returns a reason beside every refusal, one variant per `continue`/`break` in its
walk, and `CRATONVM_DBG_SCALAR_NEW` reports **every** allocation with its
verdict — `REPLACED`, `refused <reason>`, or `NOT CONSIDERED`. The first run of
it answered the page's open question outright:

```
VolumeShort2.get:  new node 23: refused Escapes(GlobalEscape)   <- the areturn
sweepVolume:       new node 61: refused LoadNotAnswerable       <- the real one
                   newarray node 62: NOT CONSIDERED
```

Three facts in six lines, and `NOT CONSIDERED` is the one a count could never
have shown.

### 1. `LoadNotAnswerable` — dominance was a whole-graph question

`program_order_proves_dominance` answers for the WHOLE graph: it is false as
soon as there is any `If`, `Merge` or multi-input `Phi` anywhere in it. A hot
loop is made of those. So the object an inliner had just brought together with
its consumer was refused for a property of the loop it had been moved *into*,
and no amount of further inlining could ever have fixed that.

The IR knows which basic block each access is in; the EA bridge was dropping it
along with the control edges (that is what the module header's "there is no CFG
here" note is about). `Graph::blocks` carries back that one fact, and
`one_block_proves_dominance` answers the ordering question for a single
allocation whose every folded access is in one straight-line block:

1. a block is straight-line, so within it ascending `NodeId` — creation order,
   program order — **is** execution order;
2. the allocation is in the block too, so each execution of the block makes a
   FRESH object and an earlier iteration's store wrote an earlier object. This
   is the clause that makes the rule safe inside a loop, and it is exactly what
   stops applying if the allocation is hoisted out of one;
3. the use walk is exhaustive — it refuses the object outright at anything it
   cannot classify — so the recorded stores are *every* store to the object.

`Op::Guard` is deliberately not a block boundary: it produces no control token,
so a null or bounds check leaves its block intact. An empty `blocks` reproduces
the old behaviour exactly, which is what makes the addition safe to carry.

### 2. `NOT CONSIDERED` — `Short2` is two allocations and only one was a candidate

The object plus its `short[2]`. An array of constant length N maps onto
`ScalarReplacementInfo::field_values` exactly the way an N-field object does and
a constant element index IS a field index, so this needed three facts the
analysis was throwing away rather than a new algorithm: `Op::NewArray` carrying
its length, an element access with a constant index bridging to
`Op::Load(i)`/`Op::Store(i)` on the array instead of the blanket `EaOp::Call`
that arg-escaped every array reference it touched, and the array admitted as a
candidate.

Fail-closed everywhere, and each refusal is named: a non-constant length, a
non-constant index, an index outside `0..len` (its `ArrayIndexOutOfBounds` throw
must survive), an `arraylength` nothing folds, a length over
`MAX_SCALAR_ARRAY_LEN` (8 — past that the trade is one allocation for a dozen
live values and spills), and `anewarray` (a never-stored reference slot would
have to be answered with `null`, and the applier's zero default is numeric). One
unresolvable access refuses the whole array, because a non-constant index keeps
the `EaOp::Call` mapping that escapes it.

Which uncovered a real latent bug on the OBJECT path: the applier materialised
one shared `Const(0)` typed `Int` for every never-stored slot, so a never-stored
`long` field forwarded a 32-bit value where a 64-bit one belongs. Zero defaults
are now per value type, taken from the load's own `ty`.

### 3. Scalar replacement exposes scalar replacement

With the array a candidate, the merged graph said:

```
new node 61: REPLACED
newarray node 62: refused StoredIntoAnotherObject
```

and that refusal is exactly right for the graph EA was looking at — the array's
only non-element use is the `putfield` that publishes it into the `Short2`. It
is also obsolete the instant that `Short2` is replaced: the applier marks the
store dead and forwards every read of the field to the array node itself, so the
array is left holding nothing but its two element accesses.

So the analysis runs to a fixed point. Bounded at three rounds, and it stops the
moment a round retires no nodes — the first round, for almost every method, so a
method with no replaceable allocation pays exactly one analysis as before. Each
round is the same sound analysis on a graph the previous round left sound;
nothing here relaxes a proof, it gives the existing proof a second look at a
smaller problem.

### 4. The deopt descriptor could not describe an array, or a nested object

Two restrictions, and both had to go before anything was actually *deleted*
rather than merely analysed.

`VirtualObjectInfo`/`VirtualObjectState` described an object as a class id plus
a field count, and the VM materializer turned that into `alloc_object_shared`.
There was no spelling of "a `short[2]`", so `virtual_object_info_for` returned
`None` for an array — which feeds `plan_scalar_replacement`'s `elide_alloc` gate
and kept every array a deopt snapshot names on the heap. Both structs now carry
`array_element_type` (`Some(atype)`, with `num_fields` as the LENGTH), and the
materializer allocates an array shell through `gc_alloc_array` and stores its
slots as ELEMENTS. Only primitive atypes reach it, so a materialized element
never needs a reference store barrier.

`frame_value_for_object` bailed the whole enclosing object when a field's value
was itself scalar-replaced ("v1 emits no nested graphs"). That is precisely this
shape: the wrapper's recipe names the array, so the moment the array became
replaceable the wrapper's own recipe was refused and the wrapper had to stay. It
now recurses, and `emitted` — which was already there for sharing — turns a
repeat into `VirtualObjectRef`, so a cycle ends at its first repeat. **The
materializer had always walked nested field graphs**; only the producer refused.

Fail-closed one level at a time: the recursion re-runs every gate on the nested
object, and a nested object that refuses itself refuses the enclosing one, with
`NestedVirtualObject` naming which level gave up.

## What is still gated

The win needs BOTH `CRATONVM_JIT_IR_INLINE=1` and `CRATONVM_SCALAR_DEOPT=1`, and
the A/B table above is the argument for why that is worth saying out loud rather
than burying: `SCALAR_DEOPT` alone measures **worse than nothing** here (267 ns
against 259), and `IR_INLINE` alone buys 1.7x. Only together do they buy 10.4x.

The reason is mechanical. `IR_INLINE` is what puts the allocation and its
consumer in one graph. `SCALAR_DEOPT` is what lets an allocation a deopt
snapshot names be deleted at all — without it `deopt_descriptor_available` is
false, `elide_alloc` is refused for any allocation a snapshot names, and this
object is named by the snapshot of the `TSeg.getShortAtIndex` call that sits
between `new Short2()` and `setX`. So on the default path escape analysis now
proves both allocations replaceable and then keeps them, which is the honest
state and is visible in the instrument.

`CRATONVM_SCALAR_DEOPT` is default-off because it "never soaked out of
default-off" (`docs/feature-designs/activate-ir-optimizer.md`), and it has a
known limitation of its own: `FrameState` in `ir_lower` hard-codes
`monitors: Vec::new()`, so an IR-tier deopt from inside a synchronized region
reconstructs without its monitors. **Flipping it is not this page's call** — it
needs the kafka/spring/tomcat/hibernate gauntlet that flag has never had. What
this page adds is a reason to run that soak: the flag is not a marginal
refinement, it is the difference between analysing an allocation away and
actually deleting it.

Regression suite, same binary, both arms: **72 passed, 0 failed**.

## Reproducing

`probes/VoxelAlloc2.java` reproduces the four-level accessor chain — a method
that RETURNS a wrapper holding a `short[2]`, over an FFM segment element read —
out of classes the probe owns, so it no longer depends on
`apps/kfusion-tornadovm/target/tornado-api-5.2.0-jdk25.jar` (a build artifact,
and the reason the original scratch probe could not be re-run).

```bash
javac -d probes/voxout probes/VoxelAlloc2.java
CRATONVM_JIT_IR_INLINE=1 CRATONVM_SCALAR_DEOPT=1 CRATONVM_DBG_SCALAR_NEW=1 \
  cratonvm --java-home <jdk25> -cp probes/voxout \
  -Dvoxel.warm=4 -Dvoxel.warmiters=20000 VoxelAlloc2
```

`voxel.warmiters` is load-bearing and is the one trap in this probe: with too
few warm invocations the arm method is only ever reached through the **OSR**
door, which does not go through IR admission at all — no splice, no escape
analysis, and the `[ir] admission` line for it simply never appears. That is
[Open 3](#still-open) below, and it is not a probe artifact: a per-frame
integration loop in the real workload has the same shape.

## Still open

1. **An OSR-only hot loop still gets nothing.** A method first entered with its
   loop already hot is compiled through the OSR door, which never reaches the IR
   admission gate, so it gets neither the splice nor escape analysis. This is
   the shape of a real per-frame sweep and is the largest remaining gap.

2. **`getIndex(III)I` cannot be lowered at the IR tier standalone.** An instance
   method with three parameters has four incoming slots, and touching one field
   turns `needs_context` on, which is the fifth — past the entry ABI's register
   file. The refusal is named now
   (`unsupported shape: incoming arg slots exceed the entry ABI registers`)
   rather than a bare `None`, and the single-pass backend already loads
   stack-resident parameters (`x64/frames.rs`, the ROUND-12 fix), so the IR
   prologue could too. It does not block this chain, because those methods are
   spliced INTO a caller whose own arity fits.

3. **`MemorySegment.getAtIndex` is a call-site intrinsic the IR tier cannot
   emit**, so `TSeg.getShortAtIndex` gets no IR body and is neither lowered nor
   splice-able. That is the whole of the remaining `rawseg` floor (23.8 ns
   against HotSpot's ~1.3), and it is tracked with the FFM element-accessor
   work, not here.

4. **A call left inside a spliced body is re-executed on a deopt**, and the
   store-locality rule proves nothing about it. Unchanged from
   `docs/jit/ir-tier-inlining.md`.

## Related

- `docs/jit/ir-tier-inlining.md` — the splice this depends on, and its own opens
- `probes/VoxelAlloc2.java` — the probe
- `jit/src/escape_analysis.rs` — `ScalarRefusal`, `one_block_proves_dominance`,
  `MAX_SCALAR_ARRAY_LEN`
