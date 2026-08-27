# kfusion's per-voxel `Short2` costs 94% of a voxel read, and escape analysis structurally cannot remove it

## Status

**Structural blocker removed; the allocation is still there.**

Three separate blockers were found. The third — a returned object escapes its
own method, so per-method escape analysis cannot touch it — is closed: the
optimizing tier can now inline the accessor chain into its consuming loop, so
the allocation and its consumer are in one graph (`CRATONVM_JIT_IR_INLINE=1`,
`docs/jit/ir-tier-inlining.md`). The measured effect on a voxel read is **~13x**
(2490-2741 ns to 171-203 ns on the probe below), and it comes from removing five
dispatch frames per voxel, NOT from deleting the object: the same run still
reports `scalar-replaced 0/2 alloc(s)`.

Two things stand between here and the object actually going away, and only the
first is understood:

  * **`Short2` is TWO allocations** — the object plus its `short[2]` storage —
    and `escape_analysis` can scalar-replace `Op::New` but not `Op::NewArray`
    (`test_new_array_local_scalar_not_replaced`). A perfect answer for the
    object still leaves the array. Array scalar replacement for a
    constant-length, constant-index array is the companion increment: an array
    of length N maps onto `ScalarReplacementInfo::field_values` exactly the way
    an N-field object does.
  * **why EA reports `0/2` on the merged graph is not yet diagnosed.** The
    structural reason it used to give (`areturn`) no longer applies.

Also fixed on the way through, and worth more than this page:
`docs/jit/ir-tier-inlining.md` records that `Op::NewArray` published a
shadow-stack push it never reloaded, which made `lower_inner` refuse **every**
C2 candidate containing a `newarray` alongside a live oop, with no reason
printed. That is what the `2 shadow pushes vs 1 reloads` line at the bottom of
this page was.

## What it costs

`VolumeShort2.get(x,y,z)` returns a fresh `Short2`, and `new Short2()` is TWO
allocations (the object plus its `short[2]` storage). kfusion's integration
sweeps 256³ voxels per frame, so that is ~33M allocations a frame from this one
call.

`probes`-shaped microbenchmark (scratch `VoxelAlloc.java`), 64³ voxels, three
arms where the middle one is the CONTROL — the same two segment reads with no
wrapper object, so the difference between it and `volume` is exactly what the
allocation costs:

| arm | CratonVM | HotSpot |
|---|---|---|
| `volume` — `VolumeShort2.get`, i.e. reads + `Short2` | **755 ns/voxel** | 2 ns |
| `rawseg` — same two reads, no wrapper | **47 ns/voxel** | 2-4 ns |
| `array` — plain `short[]` | 2.2 ns/voxel | 0.5 ns |

**The allocation is ~708 of the 755 ns — 94% of the cost.** (The 47 ns `rawseg`
floor is the FFM element accessor, already fixed separately and ~128x faster
than it was.) HotSpot pays ~2 ns because its escape analysis scalar-replaces the
same object.

## Why escape analysis does not remove it

Three blockers, found in this order. Each was measured, not inferred.

### 1. An allocation-bearing method was refused promotion to C2 — where EA runs

`c2_upgrade_would_engage` opened with:

```rust
if !scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty() || ... { return false; }
```

and that predicate gates the C1→C2 supersede, which `tiered.rs` says "produces
nearly all of the C2 compiles in a real run". Escape analysis runs only at C2.
So a method containing an allocation could never reach the tier that exists to
delete its allocations. Visible directly:

```
[ir] admission EaScope.makeAppPair(I)I: optimize=false — the C1/fast tier was requested, not C2
```

**Its stated justification was already false.** The doc gave two grounds; the
second was "the IR call eligibility loop requires `new_ops.is_empty()` anyway".
cov-04 increment 2 had already deleted that conjunct — `call_eligible` is now
`scan.anewarray_ops.is_empty()` alone, with the note "the term outlived its
reason" — and a live run says so out loud:

```
[ir] invoke-plan VolumeShort2.loadFromArray: sites=5 new_ops=1 ... call_eligible=true
```

Lifting it is now possible via `CRATONVM_JIT_C2_ALLOC_UPGRADE=1`. It is **opt-in,
not default**, for two reasons: it measured neutral here (blocker 3 dominates),
and it has an unpriced regression mechanism — at C2 a SURVIVING allocation
lowers through the shared `jit_new_object` stub where the single-pass body emits
an inline TLAB bump, so a method whose allocations escape trades a cheaper
allocation for a more optimized body. Nothing measured says which way that
lands; the flag exists so the trade can be priced on the gauntlet.

### 2. A `new` whose class is not yet loaded is deferred, permanently

`resolve_jit_new_site` deliberately never runs a user `ClassLoader.loadClass`
from inside a compile. A miss reports `JitNewSite::Deferred`, which gets no
`new_info` row, and the IR builder's `0xbb` arm then bails the WHOLE method:

```
[ir] new-site DEFERRED: .../Short2 holder=.../VolumeShort2 loader=Some(Application) loaded_anywhere=false
[ir] IrBuilder::build refused at ir.rs:5777 (bytecode pc 0)
[ir] IrBuilder::build returned None for VolumeShort2.loadFromArray — no IR body
```

`loaded_anywhere=false` is the point: `Short2` is genuinely not loaded when
`loadFromArray` is compiled — the method is compiled before it has ever run —
and the class loads moments later in the same compile (`admission
Short2.<init>()V` is two lines further down). There is **no retry**: three log
lines for that method, one attempt, single-pass for the life of the process.

This is the same shape as the code-buffer shortfall, which this codebase already
treats correctly: a transient measurement-like refusal exempted from the
permanent bail list so the next attempt can succeed. A deferred `new` deserves
the same treatment.

Confirmed by construction: adding one `vol.get(0,0,0)` to the probe BEFORE the
hot loop forces `Short2` to load, and the deferred bail disappears — the method
reaches escape analysis.

### 3. The allocation ESCAPES its own method, so per-method EA cannot help

With the class pre-loaded and EA actually running, the answer is:

```
[cratonvm-scalarnew] VolumeShort2.loadFromArray: scalar-replaced 0/1 alloc(s)
```

Zero of one, and the timing does not move (764 ns/voxel). The reason is
structural and visible in the bytecode: `loadFromArray` ends in `areturn` — it
RETURNS the `Short2`. An object that escapes its allocating method cannot be
scalar-replaced by an analysis scoped to that method, at any tier.

HotSpot removes it by inlining `get` → `loadFromArray` into the consuming loop
first; the object then dies in the caller and EA scalar-replaces it there.

## What was done, and what is left

IR-tier **inlining** of the accessor chain into the hot method, so EA sees the
allocation and its consumer in one graph — `docs/jit/ir-tier-inlining.md` for
the design, the admission set and the measurements. Blockers 1 and 2 were
prerequisites, not the fix, which is why fixing 1 alone moved nothing.

Both adjacent refusals named here turned out to matter, and both are now closed:

```
[ir] lower_inner refused: 2 shadow pushes vs 1 reloads — an unmatched push leaks the thread's shadow top
```

was `Op::NewArray` emitting a safepoint map (hence a shadow push) with no paired
`emit_shadow_reload`. Not a leak in practice — `lower_inner`'s
`shadow_pushes != shadow_reloads` check caught it and refused the method — but
that made it a SILENT coverage hole: every C2 candidate with a `newarray` beside
a live oop lost its optimized body. `Short2.<init>()V` is
`iconst_2; newarray short; invokespecial <init>([S)V`, and so was
`VoxelAlloc2.sweepVolume` once the chain was spliced into it. Fixing it is what
took the probe from 578 ns to 175 ns — the splice alone had already reached 578.

```
[ir] ir_lower::lower_inner returned None    (VolumeShort2.get, .getIndex, .<init> — no reason printed)
```

was the ABI-capacity refusal: an instance method with three parameters has four
incoming slots, and touching one field turns `needs_context` on, which is the
fifth. It is now named
(`unsupported shape: incoming arg slots exceed the entry ABI registers`) rather
than a bare `None`. It is a real and unfixed coverage limit — `getIndex(III)I`
cannot be lowered at the IR tier standalone at all — but it does not block the
chain, because those methods are spliced INTO a caller whose own arity fits.

## Reproducing

```bash
# scratch VoxelAlloc.java, compiled against tornado-api-5.2.0-jdk25.jar
cratonvm.exe --java-home <jdk25> -cp ".;<tornado-api jar>" VoxelAlloc
# engagement:
CRATONVM_DBG_SCALAR_NEW=1 CRATONVM_DBG_IR_COMPILES=1 ... VoxelAlloc
```

`CRATONVM_DBG_SCALAR_NEW` reports `scalar-replaced N/M alloc(s)` per method and
names any method whose IR build bailed; `CRATONVM_DBG_IR_COMPILES` adds the
admission verdict and (added here) the deferred `new` site with the class name,
its holder, the loader, and whether the class is loaded anywhere at all — which
is the line that separates "loader scope" from "not loaded yet".
