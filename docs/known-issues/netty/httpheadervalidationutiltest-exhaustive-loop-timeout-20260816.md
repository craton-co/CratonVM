# `HttpHeaderValidationUtilTest` — both exhaustive loops run INTERPRETED, because a `try` in the method makes the OSR door refuse it

**Status: OPEN, and this page's 2026-08-16/08-17 diagnosis was wrong.** It sized
the class as "the same compiled-code throughput wall as the sibling, with twice
the iteration count and 1.5x the work per iteration". It is not a compiled-code
wall: **the two exhaustive `@Test` methods are never compiled at all.** Measured
2026-08-17 on `perf/netty-exhaustive-loop-walls-20260817`, Windows host, release
build, G1, real-JDK mode, against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated), 2026-08-16 | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 5506 | 5506 | 0 | 39.9s |

The 39.9 s figure does not survive re-measurement, and the number that matters is
per method. `ProgressRunner` on this host, HotSpot 25:

| method | HotSpot wall |
|---|---:|
| `headerValueValidationMustRejectAllValuesRejectedByOldAlgorithm` | **54.8 s** |
| `headerNameValidationMustRejectAllNamesRejectedByOldAlgorithm` | **32.6 s** |
| both, plus the ~5504 quick parameterized subtests | **~87.4 s** |

So the 180 s per-class wall allows CratonVM **~2.06x HotSpot** on this class —
where the sibling
[`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`](httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
allows ~69x. Calling the two "the same wall" was the mis-sizing that produced
every wrong estimate below it.

## The budget

Two `@Test` methods, both annotated
`@DisabledForJreRange(max = JRE.JAVA_17)`, iterate every possible 32-bit value:

```java
int i = Integer.MIN_VALUE;
do {
    buffer.putInt(0, i);
    try {
        oldHeaderValueValidationAlgorithm(asciiString);
    } catch (IllegalArgumentException ignore) {
        assertNotEquals(-1, validateValidHeaderValue(asciiString), failureMessageSupplier);
        assertNotEquals(-1, validateValidHeaderValue(charSequence), failureMessageSupplier);
    }
    i++;
} while (i != Integer.MIN_VALUE);
```

**4 294 967 296 iterations each, 8 589 934 592 in total.** 180 s over one loop is
42 ns/iteration; over both it is **21 ns/iteration**, against HotSpot's 9.4 and
8.2. That is the real budget, and no arrangement of the existing compiler reaches
it — see [What is left](#what-is-left).

## The finding: the loops never leave the interpreter

`CRATONVM_DBG_JITC=1`, one run of the probe below:

```
OSR-recompile reason=no-cached-artifact  HeaderValidationLoopRate.headerValueLoop(I)V entry_pc=79
OSR-compile FAILED                      HeaderValidationLoopRate.headerValueLoop(I)V osr_bci=79
                                        — method marked OSR-denied for the rest of this process
OSR-recompile reason=no-cached-artifact  HeaderValidationLoopRate.headerNameLoop(I)V  entry_pc=79
OSR-compile FAILED                      HeaderValidationLoopRate.headerNameLoop(I)V  osr_bci=79
                                        — method marked OSR-denied for the rest of this process
```

A `@Test` method is invoked ONCE, so OSR is its only route out of the
interpreter. Refused, permanently, and the loop is interpreted for its whole
life. `CRATONVM_DBG=jit-method-stats` on the same run (131 072 iterations):
**`deopts=65115 c2_bailouts=65105`**,
`hot_but_stuck_in_interpreter=3 (ineligible-by-policy=3)`.

What the loops actually cost, therefore:

| | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| value loop | 9.4 ns/iter | **309 423 ns/iter** | **33 000x** |
| name loop | 8.2 ns/iter | **19 242 ns/iter** | **2 100x** |

Not the "~1.5x the sibling's per-iteration work" this page estimated. The sibling
is a compiled loop that is 2.9x too slow; this is an interpreted loop four to
five orders of magnitude too slow.

## Why: `RBC.6b`, and it is deliberate

`vm/src/runtime/interpreter/jit_bridge.rs`, `compile_osr_artifact`, refuses **any
method with a non-empty exception table** — before codegen, which is why there is
no `codegen-bail` line to find. `probes/OsrDenyShapeProbe.java` isolates it in one
run: six once-called methods with hot loops, and `try`/`catch` is the only
discriminator (an allocation before the loop, an anonymous class before the loop,
`do`/`while` instead of `for` all compile).

The refusal exists because `compile_with_param_slots` is not given an exception
table, so an OSR artifact carries no handler ranges and a callee exception
unwinding into that frame would escape a `catch` that textually guards the call —
observed once as a servlet's `try { resp.resetBuffer(); } catch (...)` silently
ceasing to catch. It must not simply be deleted.

The full write-up, the shape bisect, and the design for lifting it safely are in
[`../jit/osr-refuses-any-method-with-an-exception-table-20260817.md`](../jit/osr-refuses-any-method-with-an-exception-table-20260817.md).
That page is the one to fix; this one is a consumer of it.

## The probe, and the two wrong ways to write it

`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java` is the two
`@Test` bodies verbatim, each reached exactly once, with bounded sampling. Getting
the sampling right took three attempts, and the two failures are worth recording
because both look correct:

* **`i++` from `Integer.MIN_VALUE`, bounded** — 75.4 ns/iter on HotSpot against
  the real 12.8. A prefix window can sit entirely inside the ~5% of values
  containing `0x00`/`0x0b`/`0x0c`, which THROW and then run two `validateXxx`
  calls in the `catch`.
* **a fixed stride across the whole range** — 85.1 ns/iter, and *not* because the
  throw rate is wrong (it is 7.68%, measured, which is right). It is the branch
  predictor: the old algorithm is two data-dependent `switch`es per character over
  four characters, consecutive values make those branches near-perfectly
  predicted, and a strided walk makes them random. The real loop is contiguous.
* **contiguous WINDOWS of `i++`, window starts spread across the range** — 9.2 /
  8.1 ns/iter against the real 12.8 / 7.6. That is the shape to use.

One more thing the probe needs: **prime the two `HttpHeaderValidationUtil` entry
points the `catch` arm calls, before either loop runs.** The catch arm runs on
~7.7% of iterations, so in a bounded probe it is a cold branch the JIT sees late,
whereas in the real class the 5504 parameterized subtests have already made both
entry points hot. Without priming, HotSpot prices the catch arm at ~820 ns
against the ~74 ns the real method pays on the same VM — a 9x error that swamps
everything the probe exists to measure.

## What was ruled out, and what the eager-callee-chain fix did here

* **The `ByteBuffer.putInt(0, i)` native floor.** Still real — a heap
  `HeapByteBuffer.putInt(int,int)` is `session()` + `Buffer.checkIndex` +
  `byteOffset` + `ScopedMemoryAccess.putIntUnaligned`, of which two were
  registered natives on the ~160 ns funnel (see
  [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)).
  Thin direct helpers for `Buffer.session()` and
  `ScopedMemoryAccess.{put,get}IntUnaligned` were written on this branch and
  **reverted, because the census says they never bind**: those JDK accessor
  methods take the OPTIMIZING (IR) pipeline, whose direct-call lowering is
  register-only (`emit_direct_cross_call` requires
  `num_args + needs_context <= ENTRY_ABI_REGS.len()`, i.e. 4 on Windows), so a
  6-argument `putIntUnaligned` cannot be bound there at all and the single-pass
  and OSR binds are never reached for it. With the helpers on and off,
  `--dump-native-registry` on `NioAccessorRate` reports the identical census —
  `putIntUnaligned` 400 000 either way, `DirectByteBuffer.session()` 1 200 000
  either way. Anyone picking this up should start by giving the IR path
  stack-arg marshalling, not by writing more helpers. **It is not this page's
  wall either way**: an interpreted loop reaches no JIT bind at all.
* **Compile order.** The mechanism the sibling page was about is fixed, and it
  applies to this class too — but for the same reason, not yet visibly.
* **The 120 s JUnit method timeout not firing.** Unchanged and still not read as a
  separate defect: `common.args` sets
  `-Djunit.jupiter.execution.timeout.default=120s`, and Jupiter's default
  `SAME_THREAD` mode cannot preempt a synchronous non-interruption-checking loop.

## What is left

Two things, in order, and the first does not by itself retire this page:

1. **Precise OSR exception exits**, so `RBC.6b` can be lifted — the design is in
   the JIT page linked above. Until that lands these loops are interpreted and no
   other work on this class is measurable.
2. **21 ns/iteration.** Even fully compiled, this loop is ~10 call frames plus a
   4-byte NIO store plus a 7.7%-frequency throw/catch, and CratonVM's numbers for
   those pieces today are ~6 ns per virtual call
   (`probes/CallCostProbe.java`) and **~600 ns per throw/catch**
   (`probes/ThrowCostProbe.java`: 276 ns/iter no-throw against 900 throw-all,
   where HotSpot is 1.6). 7.7% of 600 ns is 46 ns/iteration on its own — twice the
   entire budget. So this class additionally needs a cheap throw/catch and a
   nesting inliner; it is not one fix.

An honest reading of that second item is that this page's wall is the furthest
from reach of the three `codec-http` walls, which is the opposite of what its
2026-08-17 sizing concluded — it had this class as "the harder of the two by a
wide margin and should be attacked second", correctly, but for the wrong reason
and at the wrong scale.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpHeaderValidationUtilTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

The per-method HotSpot walls, and the refusal:

```bash
java @common.args ProgressRunner \
  'io.netty.handler.codec.http.HttpHeaderValidationUtilTest#headerValueValidationMustRejectAllValuesRejectedByOldAlgorithm' \
  'io.netty.handler.codec.http.HttpHeaderValidationUtilTest#headerNameValidationMustRejectAllNamesRejectedByOldAlgorithm'

CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> @common.args \
  io.netty.handler.codec.http.HeaderValidationLoopRate 65536 2>&1 | grep 'OSR-compile'
CRATONVM_DBG_JIT_METHOD_STATS=1 cratonvm --java-home <jdk> @common.args \
  io.netty.handler.codec.http.HeaderValidationLoopRate 65536
cratonvm --java-home <jdk> -cp . OsrDenyShapeProbe 300000     # the shape bisect
cratonvm --java-home <jdk> -cp . ThrowCostProbe 200000 20     # the throw/catch floor
```

## Related

* [`../jit/osr-refuses-any-method-with-an-exception-table-20260817.md`](../jit/osr-refuses-any-method-with-an-exception-table-20260817.md)
  — what this page turned out to be, with the shape bisect and the fix design.
* [`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`](httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
  — the sibling. Its compile-ORDER mechanism is fixed; its residual is a
  non-nesting inliner. Genuinely a different problem from this one.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the native-call floor this class's `ByteBuffer.putInt` pays once per
  iteration, once the loop compiles.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — the same family of finding, with per-component throughput measurements.
