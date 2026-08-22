# The Vector API runs its generic Java fallback, ~850,000x HotSpot on GPULlama3's kernel

## Status

**OPEN**, characterised, not fixed. Opened 2026-08-22, out of
`gpullama3-model-load-and-ffm-segment-class-identity-RESOLVED-20260822.md`:
that record's defect is fixed and the application now runs its real inference
loop, which made this measurable for the first time. Before the fix the run
died on the first `matmul` and never reached the kernel at all.

## Severity

**HIGH for any Vector API workload; none for correctness.** Every value the
kernel produces is bit-identical to HotSpot — see "Correctness is not the
question" below. What does not survive is throughput: the workload that
motivated the parent record runs, and cannot finish.

## The measurement

`probes/Fp16VectorDotBench.java` prices `FP16FloatTensor.vectorDot`, the
kernel `FloatTensor.matmul` spends every token in: `ShortVector
.fromMemorySegment` out of a `MemorySegment`, `castShape` to int lanes,
IEEE-754 binary32 rebuilt by hand with `and`/`lanewise`/`or`, and
`FloatVector.fma` into an accumulator. Temurin 25.0.3+9 as the oracle.

| VM | ns per lane |
|---|---|
| HotSpot | 0.268 |
| CratonVM | 115 561 – 139 180 (four runs, JIT on) |

**~500,000x on the best CratonVM run, ~850,000x on the worst measured.** The
spread is host contention, not variance in the effect; the order does not
move.

End to end, the same gap in the application's own terms:

| VM | `LlamaApp … -p "hi" -n 1` |
|---|---|
| HotSpot | 8.2 s wall, 2.51 s of it generating the token |
| CratonVM | 56 min at 97% of one core, no token, killed |

The arithmetic agrees with the microbenchmark rather than contradicting it: a
Llama-3.2-1B forward pass is on the order of 1.2e9 lane multiply-adds, which
at 116 µs per lane is tens of hours per token. The application is not stuck
and is not looping — a `--nojit --stack-sample-ms` profile puts it in the
kernel, and RSS is flat under continuous CPU, which is what compute over
resident data looks like (the same reading error the parent record made once
already).

## Correctness is not the question

`probes/FfmVectorSegmentProbe.java` checks the same path for values and
matches Temurin 25.0.3+9 exactly — every lane of the load, the store, the
`defaultReinterpret` round trip, all eight FP16 lanes of the kernel and their
sum. `Fp16VectorDotBench` prints a `warm=` column, one `dot()` call's raw
float bits, for the same reason: it is `1061765120` on both VMs. A throughput
page that did not pin this would leave a reader wondering whether the slow
path is also a wrong one. It is not.

## Where the time goes

`--nojit --stack-sample-ms 5`, `Fp16VectorDotBench 12 2048`, **3500 samples**,
deepest frame per sample, grouped by owner:

```
 2044   58.4%  jdk.incubator.vector      (the generic Java fallback)
  914   26.1%  java.math                 (BigDecimal/BigInteger, under Math.fma)
  216    6.2%  jdk.internal.vm.vector    (VectorSupport)
  211    6.0%  other (boot, probe, JDK misc)
  115    3.3%  java.lang.Math            (fma itself)
```

93.7% is the Vector API, interpreted. The named frames are the shape of the
problem: `IntVector.bOpTemplate`, `uOpTemplate`, `lanewiseTemplate`,
`lambda$binaryOperations$13`, `IntSpecies.rvOp`, `Int256Vector.vectorFactory`,
`VectorSupport.maybeRebox`, `VectorPayload.<init>`. That is the JDK's
lane-at-a-time fallback: a lambda per operation, a fresh vector object per
result. HotSpot never runs any of it — `VectorSupport.*` is
`@IntrinsicCandidate` and C2 replaces the whole chain with SIMD instructions.

Two further costs are specific to the segment half:

* `AbstractVector.defaultReinterpret` mints a scratch `MemorySegment
  .ofArray(new byte[n])` per `reinterpretAsInts()` and round-trips the vector
  through it — `IntVector.memorySegmentSet` and the three `memorySegmentGet`
  frames together are ~9% of samples. A CratonVM segment allocation is ~5-6 µs
  (`probes/SegmentAllocBench.java`) against HotSpot's ~0.14 µs, so the scratch
  segment alone costs more than HotSpot's entire dot product.
* `MemorySegment.get`/`set` are natives, so each lane crosses the native
  boundary.

## The `Math.fma` row, and why it is NOT the lever it looks like

`java.math` at 26.1% is real: `java.lang.Math.fma` is `@IntrinsicCandidate`,
and the Java body an interpreter runs is

```java
return (new BigDecimal((double)a).multiply(new BigDecimal((double)b))
        .add(new BigDecimal((double)c))).floatValue();
```

— two `BigDecimal` constructions and a `BigInteger` Knuth division per call,
and `FloatVector.fma` calls it per lane. Measured directly with
`probes/FmaSpeed`-shaped timing: interpreted, `Math.fma` is **83-86 µs** per
call against ~0.1 µs for `a * b + c`, i.e. ~850x.

**It was implemented and then reverted, because it measures nothing.**
CratonVM already registers `fma` on `java/lang/StrictMath`, backed by Rust's
`mul_add`; adding the same on `java/lang/Math` changed the kernel by nothing
distinguishable from noise (115 561 / 139 180 with, against 116 909 / 131 483
without, interleaved). Two facts explain that and are worth recording so the
next reader does not repeat the experiment:

* **With the JIT on — the mode the application runs in — `Math.fma` is
  already ~3 ns per call**, the same as `a * b + c`. The compiler handles it.
  The 26.1% above is an artefact of the `--nojit` the sampler requires; a
  profile taken to see frames is not a profile of the run that matters, and
  this is the second time in this record's family that has bitten.
* **In the interpreter the registration does not win.** `Math.fma` has real
  JDK bytecode, and an `invokestatic` to a class that is not on
  `force_native_over_real_jdk_bytecode`'s list takes the bytecode. Making it
  win would mean force-listing a method on `java/lang/Math`, which is a broad
  change to a very hot class in exchange for a measured zero.

`probes/MathFmaProbe.java` was kept, because it is a correctness vector rather
than a performance one, and it found something: 35 of its 39 rows match
HotSpot bit for bit, and the four that do not are all `0 × Infinity`, where
CratonVM answers the canonical `0x7ff8…` NaN and the oracle answers the x86
indefinite `0xfff8…`. That is the known `Value::Double` NaN-payload limitation
(`nan-payloads-lost-to-the-compactvalue-tag-collision-20260816.md`), it
predates this work, and the probe now prints values and raw bits in separate
columns so a diff of the values is clean and the payload divergence is still
visible.

## What a fix would be

Intrinsify the Vector API over the real `jdk.incubator.vector` bytecode: bind
`jdk.internal.vm.vector.VectorSupport`'s operations to natives that work on a
whole vector, the way HotSpot's C2 does, instead of letting the
`bOpTemplate`/`tOpTemplate` lambdas run per lane.

`native-builtins/src/vector_api.rs` already implements a large Vector API
surface — `IntVector`, `LongVector`, `FloatVector`, `DoubleVector`,
`ByteVector`, `ShortVector`, `VectorMask`, `VectorShuffle`, species and
operators. It is a SYNTHETIC implementation: it registers on
`jdk/incubator/vector/FloatVector` and friends, and in real-JDK mode the
receiver is a concrete `Float256Vector`, so the real bytecode's own methods
win and none of it is reached on this path. Whether the repair is to extend
those registrations to the concrete per-shape classes, or to bind
`VectorSupport` beneath them, is the design question this page does not
answer.

It is not small, and it should not be started without deciding whether the
Vector API is a supported surface for CratonVM at all. The parent record's
application is the first workload in the tree to depend on it for throughput.

## Repro

```bash
# the kernel, both VMs
probes/Fp16VectorDotBench.java     # CratonVM: 20 2048   HotSpot: 2000 2048
# the values, which must match exactly
probes/FfmVectorSegmentProbe.java
# the segment allocation underneath defaultReinterpret
probes/SegmentAllocBench.java      # 20000
```

Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.
Compare `ns_per_lane`, and check `warm=` matches before comparing anything
else. To re-take the profile, add `--nojit --stack-sample-ms 5` and aggregate
the deepest frame of each `T19.H1 stack dump` — and read the `Math.fma` row
above before drawing a conclusion from what `--nojit` shows.
