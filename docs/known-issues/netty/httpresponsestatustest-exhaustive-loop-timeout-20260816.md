# `HttpResponseStatusTest` — the class passes now, 7% over the wall

**Status: OPEN, throughput, and down to a boundary effect.** The class no longer
hangs: on an idle host it runs **13 of 13 tests, all `ok`, in 193.8 s** against
the harness's 180 s per-class wall. Every mechanism this page has been about —
compile order, the inline chain, the non-nesting emitter — is closed or
measured; what remains is 7%, and the two levers that look biggest have both
been priced and are both dead.

Measured 2026-08-20 on `perf/netty-exhaustive-loop-residuals-20260820`,
**Azure Linux host `vm1`, idle (load 0.0)**, release build, real-JDK mode, one
binary with the flags off and on, against HotSpot 25 on the same host.

## Where the class stands

| | found | started | ok | failed | wall |
|---|---:|---:|---:|---:|---:|
| HotSpot 25 | 13 | 13 | 13 | 0 | **1.301 s** |
| CratonVM, default (everything below ON) | 13 | 13 | **13** | 0 | **193.783 s** |
| CratonVM, the 2026-08-18 inline flags OFF | 13 | 13 | 13 | 0 | 198.309 s |
| budget (the 180 s wall) | | | | | **180 s** |

Two things in that table need saying out loud, because both contradict what this
page previously reported.

**The class passes.** Earlier revisions recorded `HANG — found=0 started=0 ok=0,
process killed at the 180 s wall`, and a later one recorded `13 started, 12 ok`
with `testHttpStatusClassValueOf` failing JUnit's own 120 s `@Timeout`. Neither
is the current state. The `@Timeout` does not fire at all — Jupiter's default
`SAME_THREAD` mode cannot preempt a synchronous non-interruption-checking loop,
which the sibling page already recorded as "not a separate defect" — so the
method runs to completion and passes. The only thing standing between this class
and a green suite row is the harness's own 180 s cap, and it is **7% away**.

**The inline flags are worth 2.3% here, not 27%.** On
`probes/AssertChainProbe` the same three flags are worth **-29%/-27%**
(176.5/166.5 -> 124.4/121.2 ns/iter, interleaved, one binary). On the real class
they are worth 198.3 -> 193.8 s. That is not noise in the probe and it is not a
mis-measurement of the class; it is a structural fact this page did not know,
and it is the next section.

## The finding: an OSR artifact splices nothing, ever

`compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) hands
`x64::compile_with_param_slots` an **empty `inline_sites` map**. The inline
planner runs on the method-entry door only.

A `@Test` method is invoked exactly once, so an OSR artifact is the only
compiled form it will ever have. **Therefore no call site in
`testHttpStatusClassValueOf` is ever a splice candidate** — not
`HttpStatusClass.valueOf`, not `Assertions.assertEquals`. Everything the
2026-08-18 chain bought is collected one level down, inside the callees, where
`assertEquals(Object,Object,String)` splices `AssertionUtils.objectsAreEqual`
and that splice devirtualises its `equals`. That is real and it is why the probe
moves; it is also why the class barely does, because the loop body still pays
one full call frame for `valueOf` and one for `assertEquals` no matter what.

Confirmed by the planner's own trace rather than inferred —
`CRATONVM_DBG_JITC=1` prints `inline-planned <method> @pc=<pc>` per admitted
site, and on this loop every one of them names a callee:

```
inline-plan  pc=18 HttpStatusClass.fast_div100(I)I: DirectBind
inline-plan  pc=2  AssertionUtils.objectsAreEqual(...)Z: DirectBind
inline-planned HttpStatusClass.fast_div100(I)I @pc=18
```

and nothing at all for the OSR'd method.

### Wiring the planner into the OSR door is NOT the lever — measured

The obvious conclusion from the above is wrong, and it is worth writing down so
nobody spends a session on it. `probes/OsrVsEntryInlineProbe.java` runs one loop
body through both doors in the same process: `osrOnce` is called once (OSR is
its only route out of the interpreter), `entryMany` runs the identical body in
65536-iteration chunks so it crosses the invocation threshold and gets a
method-entry artifact — the door that DOES plan inline sites.

| round | `osrOnce` | `entryMany` |
|---|---:|---:|
| 1 | 47.16 | 47.43 |
| 2 | 49.76 | 53.99 |
| 3 | 46.44 | 47.23 |

No difference, and `CRATONVM_JIT_MAIN_INLINE=1` moves neither arm. The reason is
visible in the same trace: the only top-level site the method-entry door admits
here is `Assertions.assertEquals(Object,Object)`, a two-instruction forwarder
worth one ~4 ns frame, and `HttpStatusClass.valueOf` is not admitted by either
door. There is no 17% hiding behind the OSR door.

*(The first cut of this probe accumulated `c.ordinal()` and read 143-146 ns/iter
with every arm inside every other arm's noise. `Enum.ordinal` is a registered
native on the ~160 ns funnel — the probe was measuring the one thing it was not
asking about. The real test method calls no native. That is the third instrument
on this family of pages to have manufactured a result; the others are
`DecomposeProbe`'s 43 ns baseline and `HeaderValidationLoopRate`'s throw rate.)*

### `outer-splice-rolled-back=1` is not this loop either — measured

The previous revision ended with three candidate levers. This was the first of
them: "one admitted splice is still refused at emission. Whatever that body
contains is the next unmodelled construct, and the arm census will name it the
moment someone asks."

Asked. `try_emit_inline_site` names its rollbacks under `CRATONVM_DBG_JITC` now,
and on this loop there is exactly one, every run:

```
[cratonvm-jitc] inline-rollback java/lang/StringUTF16.newBytesFor(I)[B at pc=206:
                callee_pc=4 op=0xbc
```

`0xbc` is `newarray`, and `StringUTF16.newBytesFor` is on the string-building
path the JUnit failure-message supplier reaches — not on this loop's hot path at
all. It is not a lever, and the counter that used to raise the question now
answers it.

### `valueOf` is one virtual call, not five

The second candidate lever was "`valueOf` itself, measured at ~9-10 ns of the
~34, is five `invokevirtual contains` calls on static-final constants of
anonymous subclasses". That description is of an older netty. In the checked-out
tree (`4.2.18.Final-SNAPSHOT`) it is:

```java
public static HttpStatusClass valueOf(int code) {
    if (UNKNOWN.contains(code)) {
        return UNKNOWN;
    }
    return statusArray[fast_div100(code)];
}
```

One `invokevirtual` on `HttpStatusClass$1` (the `UNKNOWN` constant's anonymous
subclass, whose body is `code < 100 || code >= 600`), plus a `fast_div100` that
is already spliced. For the two exhaustive loops `UNKNOWN.contains` is always
true, so the array read never runs. Any future estimate that starts from "five
calls" is starting from the wrong method.

## What is fixed, and what it was worth

Kept for the record, because each of these was this page's headline at some
point:

1. **Compile ORDER** (2026-08-17). `try_jit_compile_callee_slow`'s callee
   resolver was lookup-only, so eager callee compilation was exactly one level
   deep and whether a chain ran at ~450 ns/iteration or ~80 depended,
   permanently, on the order the tiered manager reached the methods in. The
   resolver compiles transitively now. Proved by the counter, not the clock:
   `disp_calls` **2 003 538 -> 3 926** on a fixed 1e6-iteration arm.
2. **The five-step inline chain** (2026-08-18) — multi-frame deopt resume, a
   multi-frame OSR-exit transfer with admission relaxed to match, the
   single-pass scope stack, a real call inside a spliced body, and nesting.
3. **Direct-binding a spliced call** (2026-08-18), the first thing in that line
   of work to make anything faster: 6 of 6 interleaved rounds, ~45.1 -> ~39.1
   ns/iter on `AssertChainProbe`, with `disp_calls` back to ~3 870.
4. **Operand-stack merging in the inline emitter, and the devirtualisation it
   unblocked** (2026-08-18): 44.6 -> 33.8 ns/iter, monotone over five rounds.
5. **Shipping any of it** (2026-08-20). Steps 2-4 all landed behind default-OFF
   flags and nothing turned them on, so for two days the measured -24% was worth
   exactly zero to every run. `CRATONVM_JIT_INLINE_CALLS`,
   `CRATONVM_JIT_INLINE_NEST` and `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` are
   default-ON now, each with a `=0` opt-out. `CRATONVM_JIT_MAIN_INLINE` is NOT
   part of that set: adding it to the arm moves nothing (124.4 -> 123.5 and
   121.2 -> 122.4, both inside the spread), and it is the one flag with a
   documented open miscompile against it.

Correctness for the flip, one binary, flags off and on: the whole netty
`codec-http` suite, 103 classes — **identical result sets**, down to which
classes miss the wall and which two report a failure. Plus
`regression-suite/run.sh`, 64 vectors, identical in three arms.

## What is left, and it is 7%

At 193.8 s for 4 294 967 296 iterations the loop body costs **~45 ns/iteration**
against the wall's 42. Three candidate levers are ruled out above. What has NOT
been priced:

* **The call frames themselves.** `probes/CallArgCostProbe.java` prices a
  compiled static call at **4.13 ns**, one taking a reference at **6.46**, a
  virtual one at **8.19-8.96**, against HotSpot's ~0. The loop body is `valueOf`
  (one static frame containing one virtual call) plus `assertEquals` (one static
  frame, with the rest of the chain collapsed inside it) plus the `getstatic`.
  That is roughly 20-25 ns of pure call overhead in a 45 ns iteration, and the
  only way to remove it is to splice at the top level — which the OSR door
  cannot do, and which the measurement above says would not pay even if it
  could, because the sites are not admissible.
* **`MAX_INLINE_MERGE_DEPTH` (4) and `MAX_INLINE_NEST_DEPTH` (3)**, neither of
  which has been tuned against anything. Cheap to sweep, and the arm census
  (`nested-splice-refused`) would say immediately whether either binds.
* **The harness cap itself.** 193.8 s against 180 s is the smallest gap on any
  page in this family, and `class-overrides.tsv` already exists for exactly this
  — `DnsNameResolverTest` carries a 600 s entry because a class killed at the
  cap is recorded `HANG`, indistinguishable from a real deadlock. Raising this
  class's cap would make the suite row honest (13/13 `ok`) without claiming the
  throughput work is finished. That is a harness decision, not a VM one, and it
  is deliberately not taken here.

## Repro

```bash
cd apps/netty-suite-runner
timeout 900 java @common.args CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest
```

```bash
timeout 900 cratonvm --java-home <jdk> -Xmx1500m @common.args CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest
```

The same-binary A/B for the 2026-08-18 inline chain:

```bash
CRATONVM_JIT_INLINE_CALLS=0 CRATONVM_JIT_INLINE_NEST=0 CRATONVM_JIT_INLINE_SPLICE_DEVIRT=0 cratonvm --java-home <jdk> -Xmx1500m @common.args CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest
```

The probes, and the two traces that answer "which method got the splice":

```bash
cratonvm --java-home <jdk> @common.args AssertChainProbe 20000000
```

```bash
cratonvm --java-home <jdk> @common.args OsrVsEntryInlineProbe 20000000
```

```bash
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 2000000 2>&1 | grep -E 'inline-planned|inline-rollback'
```

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> @common.args AssertChainProbe 3000000 2>&1 | grep 'inline call arms'
```

## Two notes on this page's own probes, for the next reader

`probes/DecomposeProbe.java`'s `empty` arm measures **43 ns/iter** for
`sink += c` on a `static long`, so every row of that probe carries a ~43 ns
baseline that has nothing to do with the rung it names, and its `valueOf` row
additionally includes an `Enum.ordinal()` call, which is a registered native.
Read `HttpStatusClassLoopRate`, `StatusLoopArmsProbe` and `AssertChainProbe` for
this loop's cost; `DecomposeProbe`'s rows are only comparable to each other.

**`StatusLoopArmsProbe`'s `refcheck` arm was measuring the interpreter.** It
wrote `throw new IllegalStateException()` inline, which puts an `athrow` in the
method, and `RBC.6` (`has_athrow`) refused OSR for any method that `athrow`s —
so that one arm ran interpreted while its four siblings compiled. It read
**825.91 ns/iter against `full`'s 80.57**: the SUBSET arm ten times slower than
the superset it is a subset of, which is arithmetically impossible and is the
tell. Worked around 2026-08-17 by routing the failure through a callee; the
underlying refusal was narrowed 2026-08-20 (it now applies only to an `athrow`
inside a protected range, and only where compiled local handlers cannot take
it). Any arm added here must still be checked against `CRATONVM_DBG_JITC=1` for
`OSR-compile FAILED` before its number is believed.

## Related

* [`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — the sibling. Still an order of magnitude out rather than 7%, and its
  remaining wall is a different defect entirely: 1.67 `ReceiverTypeChanged`
  deopts per iteration ending in `MakeNotCompilable`, with a per-bci de-spec
  registry that is written for that reason and never read.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the per-call floor for anything reaching a registered native, which is what
  prices `Enum.ordinal` above.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — same family of finding, with per-component throughput measurements.
* `fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md`
  — the shape this looks like and is not; OSR is entered here
  (`osr_entered` non-zero, `osr_refused_entry=0`, `deopts=0`).
