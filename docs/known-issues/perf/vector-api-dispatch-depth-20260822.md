# The Vector API: the lane-at-a-time fallback is gone; the dispatch layer above it is what is left

## Status

**PARTIALLY FIXED 2026-08-22.** The defect this page opened on — CratonVM
running the JDK's generic, lane-at-a-time Java fallback for every Vector API
operation — is fixed. `VectorSupport`'s intrinsic entry points are now whole-
vector kernels in Rust (`native-builtins/src/vector_support_intrinsics.rs`),
and on GPULlama3's inference kernel they cover **100%** of calls
(`fell_back=0`, from the engagement census below).

What remains is a different thing with a different cause, and the page is kept
OPEN for it: the JDK's own dispatch layer ABOVE `VectorSupport` —
`lanewiseTemplate`, `convert0`, `ImplCache.find`, `opCode`, `sameSpecies`,
`broadcastBits` — is ordinary interpreted Java, and there are tens of those
frames per lane group.

Opened out of
`gpullama3-model-load-and-ffm-segment-class-identity-RESOLVED-20260822.md`.

## Severity

**MEDIUM** (was HIGH). No correctness consequence at any point: every value
matched Temurin 25.0.3+9 bit for bit before this change and does after.

## What moved

`probes/Fp16VectorDotBench.java`, GPULlama3's `FP16FloatTensor.vectorDot`.
The two CratonVM arms are ONE binary with the kill switch
`CRATONVM_VECTOR_INTRINSICS=0|1`, interleaved so both see the same host:

| arm | ns per lane |
|---|---|
| HotSpot | 0.268 |
| CratonVM, kernels off | 79 713 / 80 479 |
| CratonVM, kernels on | 20 813 / 21 276 |

**3.8x**, and the `warm=` column — one `dot()` call's raw float bits — is
`1061765120` on all four runs and on HotSpot.

The engagement census for the "on" arm, which is what makes that number
readable rather than a claim:

```
[cratonvm] vector intrinsics: handled=3078322 fell_back=0
[cratonvm] vector intrinsics:   binaryOp         handled=923136  fell_back=0
[cratonvm] vector intrinsics:   unaryOp          handled=153856  fell_back=0
[cratonvm] vector intrinsics:   ternaryOp        handled=153856  fell_back=0
[cratonvm] vector intrinsics:   broadcastInt     handled=461568  fell_back=0
[cratonvm] vector intrinsics:   reductionCoerced handled=601     fell_back=0
[cratonvm] vector intrinsics:   convert          handled=461568  fell_back=0
[cratonvm] vector intrinsics:   fromBitsCoerced  handled=616025  fell_back=0
[cratonvm] vector intrinsics:   load             handled=307712  fell_back=0
```

`CRATONVM_VECTOR_INTRINSICS_STATS=1` prints it; the totals line prints itself
whenever either counter moved.

## End to end: still no token, and now the arithmetic says so

`LlamaApp … -p "hi" -n 1`, the parent record's own repro, on the fixed binary:
**66 minutes at 97% of one core, no token, killed.** HotSpot does it in 8.2 s
wall, 2.51 s of it generating.

That is not a contradiction of the 3.8x and it does not need another run to
interpret. A Llama-3.2-1B forward pass is on the order of 1.2e9 lane
multiply-adds; at the measured 20.8 µs per lane that is **~7 hours per token**,
against ~40 hours before this change. The run was killed because the answer was
already computable, not because it was ambiguous.

So the kernel is 3.8x faster and the application's observable behaviour is
unchanged. Both statements are worth keeping in the same place: a speedup that
does not cross a usefulness threshold is still a speedup, and reporting it
without the threshold would be the more misleading of the two.

## What is left, and why 3.8x and not 3800x

A `--nojit --stack-sample-ms 5` profile of the same kernel, 342 samples,
deepest frame per sample. The kernels themselves are natives and so appear as
no frame at all; everything below is the JDK's own Java:

```
 48  IntVector.lanewiseTemplate
 44  AbstractVector.convert0
 19  IntVector.lanewiseShiftTemplate
 18  IntVector$IntSpecies.broadcastBits
 13  AbstractVector.sameSpecies
 11  VectorOperators$OperatorImpl.opKind
 11  VectorOperators$OperatorImpl.opCode
 11  Int256Vector.lanewise
  9  VectorOperators$ImplCache.find
  9  FloatVector.lanewiseTemplate
```

Compare that with the same profile before the change, where 93.7% was in
`bOpTemplate` / `uOpTemplate` / `vectorFactory` / `maybeRebox` and the
per-lane lambdas. Those are gone. What is there now is the ROUTE to
`VectorSupport`: operator-metadata lookups, species checks and template
methods, several dozen Java calls per vector operation.

At CratonVM's measured per-call cost of roughly 200 ns, several dozen calls
per operation is several microseconds per operation before any lane
arithmetic happens — which is the whole of the remaining 20.8 µs per lane.
**This is the general interpreter/JIT call-cost story, not a Vector API
defect**, and it is the same shape as
`filechannel-heap-read-glue-depth-20260822.md`: cost spread thinly across a
deep call chain with nothing above 15%.

Going further means intercepting HIGHER — at `Int256Vector.lanewise` and its
siblings, which are per-shape concrete classes. That is 6 element types x 5
shapes x the whole operation surface, and each registration has to decode a
`VectorOperators$OperatorImpl` to an opcode. It is a much larger and more
fragile surface than the nine static methods below, and it should not be
started without deciding whether the Vector API is a supported CratonVM
surface at all.

## What was built

Nine `jdk.internal.vm.vector.VectorSupport` entry points — the ones HotSpot
marks `@IntrinsicCandidate` — computed whole-vector in Rust: `binaryOp`,
`unaryOp`, `ternaryOp`, `broadcastInt`, `reductionCoerced`, `convert`,
`fromBitsCoerced`, `load`, `store`, plus `maybeRebox`.

The object model makes this cheaper than it sounds. A vector is exactly ONE
instance field: `VectorSupport$VectorPayload` declares `private final Object
payload` and nothing below it — `AbstractVector`, `FloatVector`,
`Float256Vector` — declares another (`javap -p`, Temurin 25.0.3+9). Reading a
vector is one field read of a primitive array; building one is an allocation
and one field write.

**Every entry point can refuse.** An opcode that is not implemented, a masked
form, an element type that will not decode, a reshape that crosses lane
boundaries — each hands the call back to the `defaultImpl` lambda the JDK
passed in, which produces exactly the un-intercepted answer. That is what
makes partial coverage safe and lets the table grow one opcode at a time.

## Correctness, and the two bugs the probe caught

`probes/VectorApiProbe.java` is new: 320 rows of raw lane bits across six
element types, four species, every implemented opcode, both shift forms,
casts, reinterprets, reductions, loads and stores — and the masked forms,
which the Rust side deliberately refuses, so those rows are the evidence that
the refusal path still runs and still agrees.

All 320 match Temurin 25.0.3+9 exactly, in both kill-switch arms.

The inputs are chosen to be where a plausible implementation is wrong, and
two of them were:

* **Sub-word shifts.** `ShortVector.lanewise(LSHR, 1)` of `Short.MIN_VALUE`
  answered -16384 where the oracle says 16384. The lane width was taken as 32
  for everything below `long`, so a sign-extended short was zero-extended at
  the wrong width and narrowed back. Java masks a short's shift count by 15
  and zero-extends to 16 bits. **Every other shift row agreed** — which is the
  argument for a probe that enumerates lane widths rather than spot-checking
  one.
* **A mask read as a lane type.** Deriving the element type from the vector
  CLASS NAME made `Long256Vector$Long256Mask` a `long` carrier, because it
  starts with "Long". A mask's payload is a `boolean[]`; building a `long[]`
  for one produced `ClassCastException: class [J cannot be cast to class [Z`
  four frames inside `LongVector.fromMemorySegment`. Exactly one row of 320
  reaches a mask through that path.

Both are pinned by unit tests in the module, which are pure predicates and
need no VM.

One row legitimately differs between VMs and is printed as `SHAPE-NOTE`
rather than compared: `SPECIES_MAX` / `SPECIES_PREFERRED` come from
`VectorSupport.getMaxLaneCount`, which is a statement about the host. Temurin
reports 256 bits on this machine; CratonVM's `getMaxLaneCount` deliberately
answers the 128-bit minimum. A probe that iterated `SPECIES_MAX` would compare
a 256-bit block against a 128-bit one and report 36 spurious differences.

## Two corrections to this page's first version

Both were wrong in the same way, and the way is worth keeping.

* It said `vector_api.rs`'s registrations are unreachable in real-JDK mode
  "because a real receiver is a concrete `Float256Vector`". **The real reason
  is that `register_vector_api_natives` is called only from
  `register_synthetic_overrides`, which does not run in real-JDK mode at all.**
  The registrations are not losing a dispatch race; they were never made.
* It said a `Math.fma` native "does not win anyway, because `Math.fma` has
  real bytecode and is not force-listed". Same error: the `fma` registration
  lives in `register_p69_misc`, reachable only from
  `register_synthetic_overrides`. It had never once been registered in
  real-JDK mode.

The lesson generalises past both: **in this tree, "the native did not win" and
"the native was never registered in this mode" look identical from the
outside**, and the registrar's call path is the thing to check first. It is
why `register_vector_support_intrinsics` is called from
`register_essential_natives_with_shims` and why that placement is stated in
the code rather than assumed.

## Repro

```bash
# correctness: 320 rows, must match a real JDK exactly
probes/VectorApiProbe.java
# throughput, both arms of the kill switch
probes/Fp16VectorDotBench.java     # CratonVM: 200 2048   HotSpot: 2000 2048
```

Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.

```bash
CRATONVM_VECTOR_INTRINSICS=0            # kernels off: the un-intercepted VM
CRATONVM_VECTOR_INTRINSICS_STATS=1      # the per-entry-point census at exit
```

The kill switch gates REGISTRATION, not each call, so the "off" arm is
bit-for-bit the un-intercepted VM rather than one that still answers
`maybeRebox` natively. Check `warm=` matches before comparing `ns_per_lane`,
and read the census before believing any speedup: "the kernels ran" and "every
call fell back while the host happened to be quieter" produce the same wall
clock.
