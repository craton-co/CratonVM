# The vectorization admission gate

Deep-research report P2, *"Add vectorization only after scalar IR stabilizes"*.
The report's claim is about ordering, not about a feature:

> Vector work before alias, range, alignment, safepoint, and deopt metadata are
> sound will multiply wrong-code risk.

Those dependencies now exist on this branch — `jit/src/scev.rs` (range,
induction variables, overflow model), `jit/src/ir.rs` (`AliasClass`,
`effect_of_node`, `may_alias`, `may_reorder`), `jit/src/x64/cpu_features.rs`
(what the host actually has). So the thing P2 asks for next is not an emitter.
It is the component that consumes all four and says **no**.

Status: **analysis only**. `jit/src/x64/vector_gate` (inside
`jit/src/x64/simd_analysis.rs`) decides whether a loop *could* be vectorized and
returns either a plan with every obligation attached or the complete list of
reasons it was refused. Nothing emits a vector instruction, nothing is wired
into `compile`, and no existing behaviour changes. The emitter's remaining debt
is itemised in [What an emitter still needs](#what-an-emitter-still-needs).

The gate admits **12 of the 29 loops** in its test corpus. That number is low on
purpose and is discussed in [The honest number](#the-honest-number).

## The transform being reasoned about

Exactly one, and everything below is stated against it: **body widening**.
Iterations `b .. b+VF` run as a single pass in which each scalar operation
becomes one whole-vector operation, in the original program order, followed by a
scalar remainder loop.

Nothing here reasons about unroll-and-jam, loop interchange, gather/scatter,
reversal, or reduction trees beyond the associativity question. A gate that
admitted a loop for "vectorization" in the abstract would be admitting it for
transforms it has not analysed.

## The dependence test

`vector_gate::dependence_between(graph, earlier, later, stride)` takes two
memory accesses **in program order** and answers `Option<Dependence>`.

### Step 1 — ask the memory model, do not guess

```rust
let conflicts = graph.may_alias(earlier.writes, later.reads)
    || graph.may_alias(earlier.reads, later.writes)
    || graph.may_alias(earlier.writes, later.writes);
```

This is `ir.rs`'s judgement, not a local one. Read/read pairs never conflict
whatever they alias. Two accesses the model proves disjoint — different storage
kinds, or two references `Graph::refs_may_alias` separates — produce no
dependence at all, and the gate never looks at their subscripts.

A hand-rolled alias check at this layer is precisely the wrong-code risk the
report names, so there is not one.

### Step 2 — only then, the affine subscripts

A distance is only meaningful between two accesses to the *same object*. If
`may_alias` says "maybe" over two different base nodes — two parameters, a
parameter and a loaded reference — that is the runtime-alias case and it has no
distance: the answer is `DepDistance::Unknown`, which the gate refuses.

With one shared base, both subscripts affine on the loop's IV as
`scale * iv + offset`, and a constant loop stride:

* the address advances by `step = scale * stride` elements per iteration;
* the two accesses sit `delta = later.offset - earlier.offset` elements apart
  within one iteration;
* `later` at iteration `k + d` touches what `earlier` touches at iteration `k`,
  where `d = -delta / step`.

If `step` divides `delta` exactly there is a dependence at distance `d`;
otherwise there is none at all (this is what separates `a[2i]` from `a[2i+1]`).

### Step 3 — the legality rule, which is about *sign*

| `d` | preserved by widening? |
|---|---|
| no integer solution | there is no dependence |
| `d == 0` | yes — within one vector pass `earlier`'s op still precedes `later`'s |
| `d > 0` | yes, at **any** lane count — program order and iteration order agree |
| `d < 0` | only when `VF <= |d|`; otherwise widening inverts the two |
| unknown | no |

The distance is kept **signed** rather than collapsed into a flow/anti/output
classification, because the classification alone does not decide the question:

* `for (i=0; i<n; i++) a[i] = a[i+1];` is an anti-dependence at distance 1 and
  widens **correctly** — the whole vector load precedes the whole vector store,
  so every lane still reads the pre-store value.
* `for (i=1; i<n; i++) { a[i-1] = k; x = a[i]; }` is *also* an anti-dependence
  at distance 1 and **does not** widen — there the store is the earlier
  operation, so the vector store clobbers what the vector load was supposed to
  read.

Both are `DepKind::Anti` at distance 1. The first is `Forward(1)`, the second is
`Backward(1)`. `kind` is reported for diagnostics; the sign decides. Two tests
pin this pair specifically
(`program_order_decides_whether_an_anti_dependence_is_fatal`,
`dependence_distance_is_signed_against_program_order`).

The lane ceiling is `min` over every `Backward(k)`. A ceiling below 2 is a
refusal (`LoopCarriedDependence`); otherwise the width is
`floor_pow2(min(isa_lanes, ceiling))`.

## Guards are per-array

`scev`'s `PreheaderGuard::LengthAtLeast` says "the guarded array is at least
this long" and names no array — the caller is the one that knows which access it
asked about, and `prove_index_in_bounds_of`'s doc says so. Three accesses into
three different arrays in the same loop produce three *identical-looking*
guards.

Deduplicating them would discharge the shortest array's obligation with the
longest array's length. That is the multi-array out-of-bounds store
`bce.rs`'s whole-method provenance pass exists to prevent, arrived at from a
different direction. The plan therefore carries `ArrayGuard { array, guard }`
and de-duplicates only within one array
(`every_array_keeps_its_own_length_guard`).

## Alignment

`analyze_alignment` computes the byte offset of the first accessed element as
`HEADER_SIZE + first_index * element_size` and asks whether it is a multiple of
the vector width, given a provable object-base alignment.

The honest value of that base alignment today is **8**:
`types/src/heap_types.rs` pins the TLAB bump grid at `HEADER_SIZE % 8 == 0` as a
compile-time assert and nothing pins it higher. `HEADER_SIZE` is 32, so the
*header* is not the obstacle — the base is. **16-byte alignment of an array's
element 0 is not provable in this VM**, and 32-byte alignment certainly is not.

Consequences, both tested:

* On x86 (`AlignmentPolicy::UnalignedOk`) this is not a refusal. `MOVDQU` is
  architecturally correct and the plan simply records `Alignment::Unknown`.
  Every plan the gate admits on the current host says exactly that.
* On a strict-alignment target (`AlignmentPolicy::NaturalRequired`, modelled by
  `VectorIsa::strict_align128`) it is a hard refusal. No backend this JIT emits
  for is one; the ISA exists so the branch is exercised rather than sitting
  untested until the first strict backend arrives.

Raising the allocator's alignment guarantee to 16 would make element 0 provably
aligned; that is a GC/TLAB change, not a JIT one, and it is not required for
correctness on x86.

## Vector width

From `x64::cpu_features`, never assumed:

| ISA | width | 32-bit int `*` / `min` / `max` | masked tail |
|---|---|---|---|
| SSE2 (architectural on x86-64) | 16 | no | no |
| SSE4.1 | 16 | yes (`PMULLD`, `PMINSD`, `PMAXSD`) | no |
| AVX2 | 32 | yes | no |
| NEON (AArch64, architectural) | 16 | yes | no |

`cpu_features` has no AVX-512 detection, so no ISA here claims 512-bit lanes or
predicate registers. That is why the only tail strategy is a scalar remainder:
`TailStrategy::ScalarRemainder { max_iterations: lanes - 1 }`, or
`TailStrategy::None` when the exact trip count is a multiple of the lane count.

An op that needs a feature the target lacks is `MissingIsaFeature`, not a
silently narrowed width.

## Floating point

Reassociating `float`/`double` addition is not value-preserving. The default is
`FpRelaxation::Strict` and a floating-point **reduction** is refused under it.
Three further facts are encoded rather than assumed:

* **Element-wise FP is admitted even under `Strict`.** IEEE-754 add/sub/mul/div
  are defined per operand pair, so a lane computes exactly the scalar result —
  including NaN payload propagation and signed zeros. `c[i] = a[i] + b[i]` over
  `double` has no accumulator and therefore no reassociation.
* **FP `min`/`max` are refused unconditionally**, relaxation flag or not.
  `Math.min`/`Math.max` on `double` order `-0.0` below `+0.0` and return NaN if
  either operand is NaN; `MINPD`/`MAXPD` return the *second* operand in both of
  those cases. That is a wrong answer, not a reordered one, so no flag reaches
  it.
* **Integer reduction is admitted under `Strict`.** JVM `int`/`long` arithmetic
  is modular two's-complement, so `+`, `*`, `&`, `|`, `^` are associative and
  commutative over the whole domain: reassociating them is exact, and `PADDD` /
  `PMULLD` wrap identically to `iadd` / `imul`.

Integer `/` and `%` are refused: they trap per element (`ArithmeticException` on
a zero divisor, and `Integer.MIN_VALUE / -1` overflows), and a lane cannot
raise.

The producer must list **every** arithmetic node in `VecArith`, not only the
ones it believes are vectorizable — an omitted operation is a silent admission.
`VecOp` therefore includes `Div`/`Rem`, which exist only to be refused.

## Overflow

Two different questions, kept apart:

1. **Index arithmetic.** Delegated entirely to `scev`. `index_span` /
   `prove_index_in_bounds_of` either prove the subscript in range or refuse with
   a `RefusalReason`, and the plan copies their `OverflowModel` verbatim.
   `NoWrapGuarded` means at least one entry of `plan.guards` is load-bearing for
   *soundness*, not merely for check elision — dropping it restores the wrap.
   Both directions are tested: an inclusive loop against `Integer.MAX_VALUE` is
   refused with `IvMayWrap`; a limit near `Integer.MAX_VALUE` is admitted with
   `NoWrapGuarded` and an `AtMost` guard carried in the plan.
2. **Element arithmetic.** Modular and therefore lane-exact, as above. This is
   the only place the gate says "overflow is fine", and it says so about a
   different quantity than (1).

## Refusal taxonomy

Every variant is a hard refusal at the stated lane count. The gate returns
**all** reasons, not the first — a gate that stops early teaches the wrong
lesson about how far a loop is from admissible.

| refusal | why |
|---|---|
| `NoVectorIsa` | the target has no modelled vector unit |
| `IrreducibleControl` | widening assumes every iteration runs the same straight-line trace |
| `VariableStride` | a runtime step has no compile-time dependence distance |
| `NonUnitStep` | `scale * stride != 1`; a contiguous load does not cover one iteration block |
| `UnknownTripCount` | `scev::trip_count` could not bound the iteration count |
| `TripCountTooSmall` | the loop may run fewer times than one vector pass covers **and no runtime check settles it** |
| `SafepointInBody` | a body safepoint must describe a frame a vectorized iteration no longer holds |
| `DeoptPointInBody` | the deopt metadata describes a scalar frame at one bytecode index; there is no encoding for "lane 3 was iteration 11" |
| `OpaqueMemoryEffect` | an unanalyzable call reads/writes `AliasClass::Any` |
| `OrderedAccess` | a volatile access or monitor op; widening would coalesce a per-iteration fence |
| `GcReferenceAccess` | a vector store of oops bypasses the write barrier |
| `UnstructuredMemoryAccess` | a field, a static, a monitor — not an array element |
| `NonAffineSubscript` | the index is not affine in *this* loop's IV |
| `MixedElementWidths` | one lane count cannot describe two widths |
| `NoMemoryAccess` | nothing to widen |
| `IndexNotProven` | `scev` refused the range or the no-wrap obligation |
| `UnknownAliasing` | the accesses may overlap and no distance exists |
| `LoopCarriedDependence` | a backward dependence below two lanes |
| `FloatReassociation` | an FP reduction under `FpRelaxation::Strict` |
| `NonIeeeVectorOp` | FP `min`/`max`, or integer `/` and `%` |
| `MissingIsaFeature` | the lane-wise instruction is not on this target |
| `ElementTooWideForVector` | one element does not fit twice in a register |
| `UnprovableAlignment` | the ISA requires natural alignment and it could not be proved |

The safepoint, deopt, GC-reference, irreducible-control, alignment and
float-reassociation rows are the six the report calls out by name. Each has a
test and each has a "must admit" twin so the refusal is shown to be about the
stated hazard and not general timidity.

### Why the GC row has no lane count that rescues it

A vector store of references writes several oop slots with one instruction and
runs no write barrier for any of them. This is the same family as the
barrier-elision use-after-free already closed on this branch: the collector's
remembered set stops describing the heap and the failure surfaces far from the
store. A reference vectorizer needs a vector-aware barrier before the gate can
be relaxed here, not a narrower width.

### Why the safepoint row is about the *body*

The back-edge poll is not a refusal. Widening lowers the poll frequency by a
bounded factor (`VF`), which delays a safepoint by a bounded amount — that is a
latency question, not a correctness one. A safepoint *inside* the body is
different: the runtime may rebuild an interpreter frame there, and the frame it
would rebuild describes one scalar iteration whose locals a widened body no
longer holds in that shape.

## Minimum trip count: a guard, not a refusal

`admit_vectorization` asks `CountedLoop::prove_trip_count_at_least(lanes, env)`
whenever the compile-time interval's minimum is below the lane count, and takes
the resulting `PreheaderGuard::TripCountAtLeast` as an obligation. It carries it
in `plan.guards` with `array == NO_NODE` — the guard is not about an array, and
that keeps it in its own de-duplication bucket so it can never discharge, or be
discharged by, an array's length guard. `vec_emit::emit_guard` already handled
this variant (`VecGuardValues::Term`); only the gate was missing.

This matters because the interval for `for (i = 0; i < n; i++)` with a runtime
`n` is `[0, i32::MAX]`, so refusing on `trip.min` refused almost every real
loop. `TripCountTooSmall` now means what is left when the proof itself refuses:
a *constant* trip count below the lane count (nothing to discover at run time —
the answer is already known and it is "no"), a decreasing or non-unit-stride
loop, a post-tested loop, or an unbounded entry value.

The tail is unaffected. A minimum is not a multiple, so
`TailStrategy::ScalarRemainder` still applies; only an exact trip count divisible
by the lane count gives `TailStrategy::None`.

## The honest number

12 of 29 corpus loops are admitted. Three caveats on reading that figure:

* The corpus is built as **must-refuse / must-admit pairs**, one pair per
  refusal class, so the ratio is partly a property of the corpus design rather
  than of real Java. It is a coverage statement, not a coverage prediction.
* It moved from 11-of-27 to 12-of-29 by *adding* the pair `TripCountTooSmall`
  never had, not by relaxing anything: a runtime-bounded loop (now admitted
  behind one compare) and a loop whose constant trip count is below the lane
  count (still refused). Asking for the trip-count witness changed no other
  corpus verdict, which is worth stating because the reason this extension was
  once deferred — "it moves the 11-of-27 number" — turned out not to be true of
  the corpus as it stood.
* Applied to real bytecode the rate would still be *lower* than the corpus
  suggests, because **every array access costs a guard**: at IR level there is no
  JVM local slot to pass to `prove_index_in_bounds_of`, so the
  `length >= a.length` tautology shortcut is unavailable and every access yields
  a `LengthAtLeast` obligation an emitter must discharge.

## What an emitter still needs

Nothing below is in scope for this pass; each is a real prerequisite.

1. **Guard emission.** All of `plan.guards`, per array, in the pre-header, with
   a deopt/fallback edge to the untouched scalar loop. A plan whose guards are
   partially emitted proves nothing and must be treated as refused.
   `vec_emit::emit_vector_loop` does this and returns the `rel32` sites the
   caller must patch; what is still missing is the caller.
2. ~~**A `TripCountAtLeast` guard shape.**~~ Done: the variant exists
   (`docs/jit/trip-count-guards.md`), `vec_emit` discharges it, and the gate asks
   for it — see [Minimum trip count](#minimum-trip-count-a-guard-not-a-refusal).
3. **Vector register allocation.** `regalloc.rs` has no XMM/YMM class for
   general values; the existing SIMD paths in `x64.rs` hand-pick registers
   inside a single pattern emitter.
4. **A remainder-loop control shape.** `TailStrategy::ScalarRemainder` names the
   plan, not the CFG. Something has to build the vector pre-loop, the entry
   check, and the scalar epilogue, and prove the two share one IV.
5. **A reduction epilogue.** An integer reduction is admitted, but a vector
   accumulator has to be horizontally folded once at exit, and that fold is the
   only place the reassociation actually happens.
6. **A producer.** Something must build `VecBodyOp` / `VecArith` from the IR
   graph — mapping `Op::ArrayLoad(MemKind)` to an element type and its index
   input to a `scev::IndexExpr`. The gate deliberately takes these as inputs so
   that a wrong producer is a refusal (`NonAffineSubscript`) rather than a wrong
   answer, but a producer that *omits* an arithmetic node is a silent admission
   and must be written to fail closed.
7. **Differential validation.** The report's required test list (NaN, overflow,
   masks, tails, deopt, GC, architecture) is covered here *at the analysis
   level*. None of it is an execution test, because there is nothing to execute.

## Out of scope

Reported rather than changed, because other work owns those files:

* ~~`jit/src/scev.rs` — `PreheaderGuard` needs a `TripCountAtLeast` variant~~ —
  landed, and consumed here.
* `jit/src/scev.rs` — `prove_index_in_bounds_of` takes `array_local:
  Option<usize>` (a JVM slot). An IR-level caller has a `NodeId`, not a slot, so
  it must pass `None` and forfeits the tautology shortcut. An overload keyed on
  something the IR can supply would remove one guard per access.
* `jit/src/ir.rs` — `AliasClass::ArrayElem` carries no element type, so the gate
  has to be *told* the `MemKind` alongside the alias class. That is why
  `GcReferenceAccess` depends on a producer-supplied field rather than on the
  memory model itself. Carrying the element type in the class (or a
  `Graph::element_kind(NodeId)` accessor) would make the GC refusal
  unforgeable.
