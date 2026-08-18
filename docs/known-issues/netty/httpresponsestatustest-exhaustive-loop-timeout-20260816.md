# `HttpResponseStatusTest` — `testHttpStatusClassValueOf` needs 42 ns/iteration and gets 92, and 63 of them are the JUnit assert

**Status: OPEN, throughput. The compile-ORDER mechanism this page was about is
FIXED (2026-08-17); the remaining gap is 2.2x and has a different cause.**
Original measurement 2026-08-16 on `3ef3eb744`; per-iteration decomposition
2026-08-17 on `cf141b8a8`; the compile-order fix 2026-08-17 on
`perf/netty-exhaustive-loop-walls-20260817` (Windows host). **The decomposition
re-measured 2026-08-17 on `perf/osr-exception-table-and-nesting-inline-20260817`,
Azure Linux host, inverts this page's estimate: the JUnit assertion chain is
~63 ns of the 92, not `valueOf`** — see
[What the remaining cost is](#what-the-remaining-cost-is--measured-2026-08-17-and-the-split-is-inverted).

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated), 2026-08-16 and still 2026-08-17 | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated), Windows host | 13 | 13 | 0 | 4.6s |
| **HotSpot 25 (isolated), Azure Linux host, 2026-08-17** | 13 | 13 | 0 | **1.925s** |

Per-test progress instrumentation confirms the process is inside
`testHttpStatusClassValueOf` when the cap fires, and nowhere else. On HotSpot that
single method is **2.607 s** on the Windows host (`ProgressRunner`,
`@@RESULT ... ms=2607`), so the 180 s per-class wall leaves CratonVM an allowance
of **~69x HotSpot** there — and **~93x** against the 1.925 s the whole class takes
on the Azure Linux host, which is where this page's later work runs. Either way
the allowance is generous and the class still hangs; the budget below (42
ns/iteration) is derived from the wall and the iteration count, so it does not
move with the host.

Contrast the sibling
[`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md),
where the same wall allows ~5.9x (re-measured on the Azure host; that page's
earlier ~2.1x came from a Windows control that does not transfer). The two pages
are not one problem at two scales, and treating them as one mis-sized both. That
page's own blocker — its two loops never compiled at all, because the OSR door
refused any method with an exception table — was closed 2026-08-17, which does
not touch this class: `testHttpStatusClassValueOf` has no `try`, so it compiled
all along and its residual below is unchanged.

## The budget, exactly

`testHttpStatusClassValueOf` (`HttpResponseStatusTest.java:116-146`) runs three
loops; the two exhaustive ones cover almost every `int`:

```java
for (int code = Integer.MIN_VALUE; code < 100; code ++) {   // ~2.147e9
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
for (int code = 600; code > 0; code ++) {                   // ~2.147e9
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
```

4 294 967 296 iterations. To fit the 180 s wall the loop body must cost
**42 ns/iteration**.

`probes/HttpStatusClassLoopRate.java` is the test method verbatim with the two
exhaustive loops bounded by `n`, called exactly ONCE so the loop is reachable
only through OSR — the shape a `@Test` method has:

| | CratonVM ns/iter | extrapolated full run |
|---|---:|---:|
| 2026-08-16 / 08-17, as first measured | 283-308 | 1214-1321 s |
| this branch's binary, `-eager-callee-chain` (the control) | **413.6** | 1776 s |
| **this branch's binary, default** | **120.3** | **517 s** |
| budget | 42 | 180 |

So the compile-order mechanism is worth **3.4x** on the real loop, and the class
is still **2.9x** over.

Both rows are one interleaved pair on the shipping binary. Take the ratio and not
the absolute: this host was running four other release builds throughout, and the
same pair measured earlier in the session gave 477.6 against 109.4 (4.4x). The
number that does not move with load is the dispatch counter below.

## FIXED: compile ORDER — a body compiled before its callee never re-binds

`probes/org/junit/jupiter/api/CompileOrderProbe.java` runs the same hot loop with
one switch: whether the JUnit callees are exercised from a *different* method
before the hot method is ever compiled.

| arm | `-eager-callee-chain` | default |
|---|---:|---:|
| cold (hot method compiles first) | 249 / 233 ns/iter | **57 / 61** |
| prewarm (callees compile first) | 78-95 ns/iter (unchanged; nothing to fix) | |

Interleaved, two rounds, one binary. The cold arm is now *faster* than the
prewarm arm used to be, which is the point: after the fix there is no cold arm.

**The mechanism, and the measurement that named it.** `CRATONVM_DBG_MIC_PROF=1`
reports the generic dispatch helper's call count, and in the cold arm it was
almost exactly one per iteration:

| arm | `disp_calls` (2e6 iterations) | `cyc_disp_total` |
|---|---:|---:|
| cold | **2 003 478** | 897 989 034 (≈448 cycles each) |
| prewarm | 4 028 | 1 237 484 |

`CRATONVM_DBG_MIC_TRACE=1` names the one site:
`org/junit/jupiter/api/AssertEquals.assertEquals(Object,Object,String)`, reached
from the two-argument overload — which the cold arm had compiled *first*.

`try_jit_compile_callee_slow`'s callee resolver (`direct_callee_lookup`) was
**lookup-only**: it bound an already-compiled callee to a raw `CALL` and
otherwise answered `callee-not-yet-compiled`, leaving the site on the generic
`jit_invoke_dispatch` round trip **for the life of the compiled body**. So eager
callee compilation was exactly ONE level deep — the mutator door compiled a
direct callee, but that callee was itself compiled through this function, whose
own statically bound sites then fell back to the helper. Whether a chain ran at
~450 ns/iteration or ~80 depended, permanently, on the order the tiered manager
happened to reach the methods in.

The resolver now compiles the callee transitively (depth ≤ 6, 96 compiles per
top-level compile, cycle-guarded through
`cratonvm_jit::jit_active_compile_contains`), so the bind is order-independent.
`CRATONVM_JIT='-eager-callee-chain'` is the same-binary A/B control.

**Proved by the counter, not the clock.** Same binary, cold arm, 1e6 iterations:
`disp_calls` **2 003 538 → 3 926**. That matters on this host, whose run-to-run
spread on a fixed configuration reaches 3x, and every absolute number on this
page moved by 1.5-2x between rounds while the counter did not move at all.

Three explanations this page carried are therefore superseded. The compile records
being identical between the arms, both of the hot method's call sites binding
`direct`, and the caller and callee bodies being byte-identical apart from baked
addresses were all *true* and all irrelevant: the differing bind was one level
DEEPER than either body, in a method neither dump covered. The MIC/PIC
"per-site runtime state" hypothesis the page ended on is not the answer either.

## What the remaining cost is — MEASURED 2026-08-17, and the split is inverted

The decomposition below replaces the estimate this section used to carry, and it
reverses it. Azure Linux host, release build, real-JDK mode, current dev binary.

**`probes/StatusLoopArmsProbe.java`** — the loop with one rung replaced at a
time, every arm a separate once-invoked method:

| arm | HotSpot ns/iter | CratonVM ns/iter |
|---|---:|---:|
| bare | 0.51 | 1.07 |
| getstatic | 1.09 | 1.56 |
| valueOf | 0.86 | **17.71** |
| full (`assertEquals`) | 12.51 | **80.57** |

**`probes/AssertChainProbe.java`** — the assertion chain split rung by rung,
each row adding exactly one level to the row above, `n = 2e7`:

| rung | HotSpot ns/iter | CratonVM ns/iter | CratonVM delta |
|---|---:|---:|---:|
| bare (control) | 0.51 | 0.80 | — |
| + `valueOf` | 1.19 | 18.29 | **+17.5** |
| + reference compare | 0.63 | 17.36 | ~0 |
| + `UNKNOWN.equals(k)` | 0.77 | 28.12 | **+10.8** |
| + one more static rung | 0.80 | 36.03 | **+7.9** |
| + the real `Assertions.assertEquals` | 4.91 | 91.55 | **+55.5** |

So the split is **`valueOf` 17.5 ns and the JUnit assertion chain ~63 ns**, not
"`valueOf` ~30 ns plus four more frames". The chain is 78% of the cost and is the
thing standing between this class and its budget. HotSpot's whole chain is
~11.7 ns on the same probe, and 4.91 ns/iter end to end.

Note also that the class is **91.55 ns/iteration on the current binary, not
120.3** — 393 s extrapolated against the 180 s wall, so the gap is **2.2x**, not
2.9x.

### Three things that are NOT the cause, each ruled out by a counter

* **The generic dispatch helper.** `CRATONVM_DBG=mic-prof` on the assert loop:
  `disp_calls=3776` over 2 000 000 iterations. The eager-callee-chain fix above
  is working and the chain is direct-bound.
* **A rung left interpreted.** `CRATONVM_DBG=jit-method-stats` on the same run:
  `hot_but_stuck_in_interpreter=0`, `c2=18`, `compiles: c1=18 c2=20 osr=3`.
  Every frame in the chain is compiled.
* **Reference arguments.** A compiled call that passes oops must spill them and
  publish an oop map, which an int-only call need not, so it was worth pricing.
  `probes/CallArgCostProbe.java` says it is worth ~2 ns, not the ~14 ns per
  frame the chain shows. The hypothesis is dead; do not re-run it.

### The per-call floor, measured

`probes/CallArgCostProbe.java`, deltas over its own no-call control:

| | HotSpot | CratonVM |
|---|---:|---:|
| static, no args | ~0 | **4.13** |
| static, 1 int | ~0 | 4.16 |
| static, 2 ints | ~0 | 5.41 |
| static, 1 reference | ~0 | 6.46 |
| static, 2 references | ~0 | 7.51 |
| virtual, 1 int | ~0 | 8.19 |
| virtual, 1 reference | ~0 | 8.96 |

HotSpot's whole column is ~0 because it inlines all of them; the negative deltas
there are noise around a loop that has been optimised to nothing.

**This is the whole argument for what is left.** One iteration of this test is
~10 real call frames. At a measured floor of 4.1 ns per static call and 8.2 per
virtual one, ten frames cost 40-80 ns before any of them does any work — and the
budget for the entire iteration is 42 ns. **No arrangement of real calls fits.**
The only lever is not making the calls, i.e. inlining, which is what HotSpot
does and what the numbers above say it is worth. (Supersedes the earlier
`CallCostProbe` figures of 0.84 ns per static call and 6.20 per virtual: that
probe's arms are shaped so its callees inline, so it prices a call that does not
happen.)

## Why the inliner cannot do it, and in what order that is fixable

**The single-pass emitter splices LEAF bodies only**, and there are two
independent gates, not one:

1. `resolve_inline_site_from` (`vm/src/runtime/interpreter/jit_bridge.rs`)
   rejects a callee containing `invokevirtual` / `invokestatic` /
   `invokeinterface` outright, so no such site is ever planned.
2. `try_emit_inline_body` (`jit/src/x64/inlining.rs`) has no arm for those
   opcodes either — they hit its catch-all bail. The one invoke it admits is a
   resolver-proven elidable super-`<init>`, which emits no code at all.

So both `valueOf` (five invokes) and every rung of the assertion chain are
ineligible at every door. Measured on the real-loop probe:
`CRATONVM_JIT_MAIN_INLINE=1` 96.8 ns/iter,
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1` 94.9, both 108.9, neither 92.8 — all
inside each other's noise. Turning inlining knobs on cannot help while the
inliner cannot nest.

**And the thing that must land first is not the inliner — nor even the inline
metadata.** `try_emit_inline_site` refuses, as a *postcondition*, any spliced
body that published deopt metadata:

> Every inlined body … is entered and left inside ONE frame, the caller's own,
> and deopt metadata has no way to say otherwise: `deopt::FrameState::caller`
> exists but no producer populates it, so an inlined scope is not representable.
> A deopt point published from inside a spliced body would therefore name the
> CALLER's method with the CALLEE's bci.

A callee containing a real call publishes exactly that — `emit_post_invoke_
exception_check` records a reason-9 point at the callee's bci.

The obvious reading is "record the scope, then relax the postcondition". **That
is wrong for this class, and the reason is specific to it: the method that needs
inlining here is a `@Test` body, invoked ONCE, so OSR is its only door out of the
interpreter.** And an artifact carrying an inlined caller scope cannot be
OSR-entered at all:

* `CompiledMethod::osr_exit_policy` refuses any deopt point with
  `frame_state.caller.is_some()` (`OSR_REFUSE_INLINED_SCOPE`), because
* the VM's in-place OSR-exit transfer is single-frame —
  `transfer_osr_exit_into_live_frame` bails on `"inlined caller chain"`, and so
  do `resume_from_ir_deopt` and `build_deopt_frame_inner`. Its own comment says
  "Lift this the same day that transfer grows a multi-frame path."

So recording scopes first would make **exactly the artifact that needs inlining
un-enterable**, and the loop would run interpreted — strictly worse than not
inlining at all. That constraint was prose until 2026-08-17; it is now pinned by
`a_deopt_point_with_an_inlined_caller_scope_refuses_the_osr_entry`
(`jit/src/lib.rs`), which also asserts the same artifact without the scope IS
admitted, and which fails if either refusal arm is removed.

The order is therefore:

1. ~~VM-side multi-frame deopt resume.~~ **STARTED 2026-08-18 — done for the
   deopt-exit sink (`resume_from_ir_deopt`), which is the one that materialises
   fresh frames.** `materialise_inlined_chain` builds the whole chain
   outermost-first or refuses it, and `push_inlined_chain` pushes what it built;
   the split is structural because a deopt that half-materialises a chain has no
   recovery. A refusal still falls back to the whole-method re-run, so the worst
   case is the old behaviour. Three things it had to get right, each with a test
   that fails when its guard is removed:
   * a caller scope parks at the invoke's **successor**, not at its bci — the
     call is already in progress (`RESUME`, not `REEXECUTE`), and this is the
     computation `jit/src/lib.rs` says it cannot do because it has no bytecode;
   * `Unsupported` in a caller's locals **refuses**, where the in-place OSR
     transfer tolerates it — a materialised frame has no existing value to leave
     alone, and every sink maps a missing slot to `Int(0)`;
   * an inlined callee is resolved in the loader context of the scope that
     encloses it, through `MemberResolver` — `method_key` is a bare string with
     no VM and no loader, which is the ambiguity that door exists to close.

   Still to do here: the other two sinks (`build_deopt_frame_inner` and the
   OSR-exit transfer, which is item 2 below).
2. ~~Multi-frame OSR-exit transfer, then relax `osr_exit_policy`.~~ **DONE
   2026-08-18.** `transfer_osr_exit_chain_into_live_frame` handles the two
   halves a chain has, which are not alike: the outermost scope IS the OSR'd
   method, so its frame already exists and is written in place, parked at the
   SUCCESSOR of its invoke; every scope beneath it is pushed. All the fallible
   work happens before the live frame is touched, because a partial success here
   would leave a live frame describing one method and a pushed frame describing
   another.

   Admission relaxed exactly as far: a deopt point may carry a chain up to
   `MAX_OSR_INLINE_RESUME_DEPTH`, defined once in the jit crate and consumed by
   the VM so the two cannot drift. The **entry contract** keeps its blanket
   refusal — an OSR entry pc is always an outer-scope block start, so a contract
   naming an inlined scope is malformed rather than deep. Those were two rules
   wearing one tag, and 08-17's pinning test caught its own drift by continuing
   to pass for the wrong reason after the relaxation.
3. **Inline scopes in deopt metadata** — give `FrameState::caller` a producer.
   The IR-side representation is already built and tested
   (`docs/jit/deopt-inline-scopes.md`: `InlineScopeTable`, `caller_chain_for`,
   `lower_inner_with_scopes`, chain-aware `frame_state_is_resumable`); what is
   missing for THIS backend is the single-pass scope stack, "pushed at the splice
   and popped at the callee's return", replacing
   `build_and_record_deopt_point`'s hard-coded `caller: None`.
4. **A real call inside a spliced body.** With scopes recorded and resumable,
   `try_emit_inline_body` can emit the ordinary dispatch/direct-call sequence for
   `0xb6`/`0xb8`/`0xb9` instead of bailing, and the postcondition above relaxes
   from "published any metadata" to "published metadata with no caller scope".
   Note this needs BOTH gates opened: `resolve_inline_site_from` rejects those
   opcodes outright too, so a site is never even planned.
5. **Nesting.** `InlineSite` grows a `nested_sites: HashMap<callee_pc,
   InlineSite>`, `resolve_inline_site_from` fills it recursively under a depth
   budget, and the emitter recurses. Statically bound callees are the tractable
   first cut and are most of what this class needs — the assertion chain's first
   four rungs are all `invokestatic`. `valueOf`'s five `contains` calls are
   `invokevirtual` on static-final constants of anonymous subclasses, so they
   additionally need devirtualisation with a guard.

Steps 1-4 are correctness-critical, and their failure mode is a silent wrong
stack rather than a slow loop. That is the honest size of "needs an inliner that
can nest", and the inliner is the last item on the list rather than the first.

## What is left, in order

1. The five-step chain above, in that order — multi-frame resume, multi-frame
   OSR transfer, inline scopes, calls inside spliced bodies, nesting. It is the
   only item that can close the 2.2x, and its first two steps are VM work rather
   than compiler work.
2. `Enum.equals` at **10.8 ns for one virtual call** (`AssertChainProbe`) against
   a measured 8.2-9.0 ns virtual-call floor — so it is a plain virtual call and
   nothing more, which retires this page's earlier "37 ns, six times a compiled
   virtual call" reading. `Enum.ordinal()` and `Object.equals` remain registered
   natives on the ~160 ns funnel
   ([`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)),
   but this chain reaches neither.

## Two notes on this page's own probes, for the next reader

`probes/DecomposeProbe.java`'s `empty` arm measures **43 ns/iter** for
`sink += c` on a `static long` — so every row of that probe carries a ~43 ns
baseline that has nothing to do with the rung it names, and its `valueOf` row
(312 ns) additionally includes an `Enum.ordinal()` call, which is a registered
native. Read `HttpStatusClassLoopRate`, `StatusLoopArmsProbe` and
`AssertChainProbe` for this loop's cost; `DecomposeProbe`'s rows are only
comparable to each other.

**`StatusLoopArmsProbe`'s `refcheck` arm was measuring the interpreter, and said
so out loud if anyone had read it.** It wrote `throw new IllegalStateException()`
inline, which puts an `athrow` in the method, and RBC.6 (`has_athrow`) refuses
OSR for any method that `athrow`s — so that one arm ran interpreted while its
four siblings compiled. It read **825.91 ns/iter against `full`'s 80.57**: the
SUBSET arm ten times slower than the superset it is a subset of, which is
arithmetically impossible and is the tell. Fixed 2026-08-17 by routing the
failure through a callee. Any arm added here must be checked against
`CRATONVM_DBG_JITC=1` for `OSR-compile FAILED` before its number is believed.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpResponseStatusTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

```bash
cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 10000000
cratonvm --java-home <jdk> @common.args StatusLoopArmsProbe 10000000
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 cold
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 prewarm
# the A/B control for the fix, and the counter that proves it
CRATONVM_JIT_EAGER_CALLEE_CHAIN=0 CRATONVM_DBG_MIC_PROF=1 \
  cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 1000000 40 cold
```

The per-method HotSpot wall — the number the 42 ns budget should be compared
against — comes from the progress launcher, not the suite harness, which prints a
line only for failing tests:

```bash
java @common.args ProgressRunner \
  'io.netty.handler.codec.http.HttpResponseStatusTest#testHttpStatusClassValueOf'
```

## Related

* [`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — sized as "the same mechanism at twice the iteration count". It is not the same
  mechanism: those loops never compile at all. That page carries the correction.
* `fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`
  — the defect the sibling page turned out to be, fixed 2026-08-17. This loop has
  no `try`, which is why it is compiled and merely slow.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the per-call floor for anything reaching a registered native, which is what
  prices `Enum.ordinal`/`Object.equals` above.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — same family of finding, with per-component throughput measurements.
* `fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md`
  — the shape this looks like and is not; OSR is entered here
  (`osr_entered` non-zero, `osr_refused_entry=0`, `deopts=0`).
