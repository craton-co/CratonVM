# A compiled call reaches its compiled callee through a Rust helper — two causes, both counted

**Status: OPEN.** Filed 2026-08-17 out of
`internal/performance/vm-per-call-dispatch-cost-RETIRED-20260817.md`, which
this page is the successor to. That page asked "where does the per-call cost
go" and answered it with a profile; this one starts from the census that
replaced the profile, and both causes below are stated as **counts**, so
neither needs a quiet box to re-check.

The shape is one sentence: **a compiled Java method calls another compiled Java
method by leaving compiled code, entering a Rust dispatch helper, looking up an
entry pointer the helper already has cached, and calling it.** On netty's
`BigEndianHeapByteBufTest` that happens **111 543 628 times in a 55 s run** —
98.4% of every `jit_invoke_dispatch` call, and essentially the whole of what
the retired page counted as `jit_entries`.

```text
[DISP_CENSUS] kind_static=107874082 kind_special=5475486 kind_virtual=173 kind_interface=116
              out_dcache=111543628 out_site_native=1507946 out_tail=259051 ...
```

Read `out_tail` before anything else: **0.23%** of these reach
`invoke_or_native`. The registry, the `(class, method, descriptor)` cascade and
the native funnel are not on this path. Two things are.

## Cause 1 — a call site is offered a direct `CALL` exactly once

`CRATONVM_DBG=intrinsic-stats`, same class:

```text
[cratonvm] direct callee binds: 666 bound, 1394 left on the dispatch helper
```

A statically bound site (`invokestatic` / non-`<init>` `invokespecial`) is
offered a direct machine `CALL` while its **caller** is being compiled. The
ladder asks `callee_compiler` for the callee's entry; if the callee is not
compiled at that instant it hands back nothing, the site is emitted against
`jit_invoke_dispatch`, and **nothing ever revisits the decision**. The callee
then compiles moments later — which is precisely what `out_dcache` is counting,
111.5 M times.

**68% of the sites that asked got nothing**, and 95% of the helper's traffic is
`invokestatic`. This is not a property of the call; it is a property of the
order in which two methods happened to cross their compile thresholds.

**What a fix needs.** Re-binding a site after its callee is live — either by
recompiling the caller once its callees are published, or by patching the
emitted `CALL` in place. Both are codegen work. The counter above is the
acceptance criterion: a fix that does not move `1394` toward `0` has not landed,
whatever the clock says. Note the existing note on callee tier-up
(`project_callee_tierup_regression_20260730`) is in this family.

## Cause 2 — a callee that declares an exception table is barred from the inline cache

`probes/NativeFunnelFloorProbe.java` carries two Java-callee rungs that differ
by **one never-taken `try`/`catch` in the callee** and nothing else. ABBA, ONE
binary, ONE gate (`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH`):

| rung | barred (default) | publish ON |
| --- | ---: | ---: |
| interface call, callee has no exception table | 21.64 / 21.18 | 19.76 / 19.45 |
| interface call, callee has `try`/`catch` | **221.72 / 218.61** | **19.30 / 18.55** |

**11.4x**, with the control rung unmoved. That is the whole Rust helper route
measured against the inline machine-code cascade the callee is barred from.

**The bar is a correctness bar, not an oversight.** The cascade emitted by
`jit/src/x64.rs` loads the raw entry into `R11` and `CALL`s it; nothing on that
route can run `route_implicit_exc_through_callee`, so a callee that returns the
deopt sentinel for an exception its OWN table should catch would have that
sentinel read by the compiled CALLER as its own deopt. `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1`
is a measurement switch, **not a fix**, and must not be defaulted on.

**What a fix needs.** The cascade must recognise the sentinel after the `CALL`
and route it through the callee's own table — a compare-and-branch to a stub
plus the Rust routing the helper path already performs. RBC.6
(`feature-designs/jit-local-exception-handlers.md`) relaxed the *compile* gate
for these methods and explicitly added **no new codegen**, so the machine-code
half of that story is still missing; this is it.

Note the population: every `AbstractByteBuf` bounds check, every javac
`finally`, every `try`-wrapped library method. On Tomcat this is the same
family as `Response.toAbsolute()`.

## What was already taken off the Rust route

The retired page's branch removed five round trips from the helper itself
(`perf/per-call-dispatch-residuals-20260817`), worth **-4% to -9% CPU** on
`BigEndianHeapByteBufTest` across two non-overlapping ABBA rounds — and
**nothing measurable** on `AdaptiveByteBufAllocatorTest`, where a fully
interleaved ABBA put the baseline at 269.07 / 271.85 s of CPU and the fixed
build at 267.97 / 279.71 s, 127/127 in every arm.

That null result is the argument for this page. The helper's own cost has now
been shaved five times and the class the whole investigation was opened for does
not move: **the helper is not expensive, it is unnecessary.** Both causes above
are about not entering it, and neither can be attacked without codegen.

## Repro

```bash
cd apps/netty-suite-runner
CLS=io.netty.buffer.BigEndianHeapByteBufTest

CRATONVM_DBG=mic-prof,intrinsic-stats <cratonvm> --java-home <jdk25> --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner $CLS 2>&1 \
  | grep -E "DISP_CENSUS|direct callee binds"

# cause 2, on ONE binary
<cratonvm> --java-home <jdk25> --Xmx 1500m -cp <out> NativeFunnelFloorProbe 1000000 20
CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1 <cratonvm> ... NativeFunnelFloorProbe 1000000 20
```

## Measurement note

Every number on this page is a count or an in-process ratio. The class-level
clock on the shared Azure host is not usable for effects of this size: the same
binary measured 52.3 s, 56.9 s, 67.8 s and 72.5 s of CPU on this class in one
afternoon. Size anything here with the counters first.
