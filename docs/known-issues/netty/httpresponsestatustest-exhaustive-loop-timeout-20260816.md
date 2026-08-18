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
3. ~~Inline scopes in deopt metadata.~~ **DONE 2026-08-18.** The single-pass
   backend has a scope stack: `push_inline_scope` captures the enclosing frame
   at the invoke (dropping `callee_num_args` stack slots, because a caller scope
   is parked mid-`invoke`), and `build_and_record_deopt_point` fills
   `FrameState::caller` from it instead of hard-coding `None`.
   `try_emit_inline_site`'s postcondition now asks whether a published point
   *says* it came from inside a splice, rather than whether anything was
   published at all.
4. ~~A real call inside a spliced body.~~ **DONE 2026-08-18 — and it is a
   PESSIMISATION on its own. Measured, not predicted.**

   Both gates are open: `resolve_inline_site_from` records every
   `invoke{virtual,static,interface}` in a candidate body and resolves it
   against the CALLEE's constant pool into `InlineSite::invoke_targets`
   (`InlineInvokeTarget` — name triple, receiver-included argument count,
   return byte, dispatch kind, and the CALLEE's declaring class id, which is
   what keeps a two-loader duplicate resolving through the right copy);
   `try_compile_inner` interns each into this compile's own
   `_jit_strings`/`_jit_invoke_infos` arenas; and `try_emit_inline_body` has the
   arm. A non-elidable `invokespecial`, which used to reject the site outright
   because elision was the only 0xb7 arm, now takes the same route.

   The substantive half was, as this page said, the resolution: the top-level
   `invoke_info` is keyed by CALLER pc and could not be reused. What this page
   did NOT anticipate is what the emitted call costs.

   | arm (one binary, three interleaved rounds, `probes/AssertChainProbe`) | `assertFull` ns/iter |
   |---|---:|
   | base | 47.2 / 45.3 / 47.9 |
   | + `CRATONVM_JIT_MAIN_INLINE` | 50.3 / 79.8 / 58.9 |
   | + `CRATONVM_JIT_INLINE_CALLS` (dispatch fallback) | **163.3 / 266.6 / 172.2** |
   | + `CRATONVM_JIT_INLINE_NEST` | 47.3 / 59.6 / 80.2 |
   | HotSpot | 3.15 / 2.01 / 2.57 |

   `sink` is byte-identical in every arm, so this is a speed result and not a
   correctness one. `CRATONVM_DBG=mic-prof` over 2 000 000 iterations names the
   mechanism outright:

   | | base | + inline calls |
   |---|---:|---:|
   | `disp_calls` | 3 870 | **2 003 361** |
   | `cyc_disp_total` | 1.86M | **1 049M cycles** |

   One blind dispatch per iteration, ~524 cycles each — which is the entire
   163-266 ns.

   **The premise this step was built on is inverted.** This page's own
   "Three things that are NOT the cause" section already recorded
   `disp_calls=3776`: the chain is *already direct-bound*, every rung a raw
   `CALL` to a compiled entry. So splicing the enclosing body removes one frame
   worth ~4 ns and converts the call inside it from that direct `CALL` into
   `jit_invoke_dispatch`, which resolves by name on every execution, worth
   ~175. Splicing a body is only worth doing when the call inside it does not
   get worse — and the per-call floor table above is exactly the evidence that
   should have predicted this, read in the other direction.

   The dispatch fallback is therefore opt-in
   (`CRATONVM_JIT_INLINE_CALL_DISPATCH`, default OFF, kept because it is the arm
   that reproduces the table above). With it off, a callee containing a call is
   admitted only when every one of those calls is itself spliced, and
   `invoke_targets` is then cleared — which removes the emitter's fallback too,
   so a nested splice that bails during emission bails the enclosing splice
   rather than silently degrading to the helper. That makes the feature
   monotone: with the fallback off, `CRATONVM_JIT_INLINE_CALLS` cannot make
   anything slower than not setting it.

   Deopt safety needed no new rule, and neither of the two things this page
   previously listed as blockers is one:

   * *The deopt-metadata postcondition.* Measured at step 3: no splice publishes
     a deopt point, structurally. The invoke arm deliberately omits
     `snapshot_pre_intrinsic_call` — it keys a point by bci, and inside a splice
     the only bci available is the callee's, a different bytecode space from the
     one the artifact's metadata is indexed by. So the postcondition still
     holds, and it is now a ratchet rather than a gate.
   * *The exception-check stub's throw pc.* `dbg_last_pc` is assigned in exactly
     one place — the outer bytecode walk — and `try_emit_inline_body` never
     touches it, so throughout a splice it holds the CALLER's invoke pc. That is
     the correct attribution: an exception escaping an inlined body belongs to
     the call site in the enclosing method, whose exception table is the one to
     search. The already-shipped spliced `getfield` / `getstatic` / `arraycopy`
     sites rely on the same thing.

   Still refused, unchanged: a callee with a non-empty exception table of its
   own, because splicing one would need the caller to carry its ranges.

5. ~~Nesting.~~ **DONE 2026-08-18.** `InlineSite::nested_sites`,
   `resolve_inline_site_from` recursing on its own callee's statically-bound
   calls under `cratonvm_jit::MAX_INLINE_NEST_DEPTH` (3 — what
   `assertEquals(int,int)` -> `assertEquals(Object,Object)` -> `objectsAreEqual`
   needs), and `try_emit_inline_body` recursing through
   `try_emit_nested_inline`. Nested sites are ADDITIVE with the dispatch entry
   when the fallback is enabled, so a nested body that bails mid-emission drops
   to the ordinary call instead of failing the outer splice.

   Only statically bound calls nest. `invokevirtual`/`invokeinterface` inside a
   spliced body cannot: selecting a body needs a runtime receiver, and the
   receiver-type profile is keyed by the ENCLOSING method's bci, not a
   callee-internal pc. That is why `valueOf`'s five `contains` calls
   (`invokevirtual` on static-final constants of anonymous subclasses) are still
   out of reach — they need per-splice devirtualisation with a guard, which is
   PGO-02's machinery re-keyed.

   Nesting recovers the step-4 regression (47.3 in round 1 against a 47.2 base)
   but does not beat the baseline. The reason is visible in the same table: the
   chain's first rungs collapse, and the terminal `UNKNOWN.equals(k)` —
   `invokevirtual`, so un-nestable — keeps its frame. Removing three ~4 ns
   direct calls out of an ~47 ns iteration is inside this probe's round-to-round
   noise, which is itself ~±15 ns on this host.

### Direct-binding a spliced call — LANDED 2026-08-18, and it is the win

The lever the step-4 measurement named, and the first thing in this whole line
of work that made the class faster.

`resolve_inline_site_from` now consults the SAME direct-bind resolver the
top-level `direct_calls` planning uses (`callee_compiler` on the mutator door,
`direct_callee_lookup` on the background one), so a call inside a spliced body
is emitted as a raw `CALL` to the callee's compiled entry instead of the blind
`jit_invoke_dispatch`. Every refusal gate applies unchanged and in the same
order, and the baked entry is registered in
`CompiledMethod::_direct_callee_entries` — which is what pins the callee
artifact and what the invalidation reverse closure walks. Omitting that
registration has no symptom until the callee tiers up, so a test asserts it.

`probes/AssertChainProbe`, one binary, six interleaved rounds, `assertFull`:

| round | base | + direct-bound spliced calls |
|---|---:|---:|
| 1 | 45.56 | **39.12** |
| 2 | 44.45 | **40.19** |
| 3 | 45.45 | **39.13** |
| 4 | 44.78 | **39.04** |
| 5 | 68.98 | **38.97** |
| 6 | 44.35 | **39.00** |

**6 of 6 rounds faster, ~45.1 -> ~39.1 ns/iter (-13%)** — and far more stable
(range 1.2 ns against the base's 24.6), which is what removing dispatch round
trips looks like. `CRATONVM_DBG=mic-prof`: `disp_calls` is ~3 870 in every arm
now, including the arm that still permits the blind-dispatch fallback. The
2 003 361 blind dispatches step 4 introduced are gone.

**And the class moved.** codec-http, 93 classes, same binary, flags off vs on:

| | `HttpResponseStatusTest` |
|---|---|
| off | `HANG` — `found=0 started=0 ok=0`, process killed at the 180 s wall |
| on | `found=13 started=13 ok=12 failed=1`, 171 s |

12 of its 13 tests now run and pass where the whole class was previously an
opaque hang with nothing started. The one remaining failure is
`testHttpStatusClassValueOf` hitting JUnit's own 120 s `@Timeout` — this page's
exhaustive loop, still over budget, which is what a 13% improvement against a
2.2x gap predicts. Every other class in the suite is unchanged (87 PASS, same
set, same per-test counts).

### Devirtualising inside a splice — LANDED, MEASURED, and INERT

Also implemented: `NestedInlineSite::guard_class_id`, a receiver class-id guard
around a nested splice with the miss edge taking the ordinary call —
PGO-02's shape one level down (`emit_guarded_nested_inline`).

Both pages predicted this needed "a profile keyed by (caller pc, callee pc)".
That was the wrong shape: receiver types are recorded against the bci of the
method that is EXECUTING, so a call inside `objectsAreEqual` is already profiled
under `objectsAreEqual`'s own `MethodKey` at its own bci — exactly the (method,
pc) pair a nested site names.

**It fires zero times, and the engagement counters are what say so.** Timing put
`devirt` within noise of `nest` (41.1/39.2 against 39.4/39.5), which is
consistent with two completely different findings. `CRATONVM_DBG=jit-method-stats`
now prints, unfiltered:

```
inline call arms: spliced-call-direct=12 spliced-call-dispatch=0
                  nested-splice=0 nested-splice-guarded=0
                  nested-splice-guarded-refused=0
```

`spliced-call-direct=12` is the win above. `nested-splice=0` and
`nested-splice-guarded=0` mean **nesting has never fired either** — so the
step-5 "recovers the baseline" reading from 2026-08-18 was measuring the
direct-bind path, not nesting. `nested-splice-guarded-refused=0` further says
these were refused at the RESOLVER, not at emission.

`CRATONVM_DBG_JITC` names each refusal, and there are two causes:

* **Profiling is default-OFF** behind `CRATONVM_TIER_PGO`, so the first
  devirt measurement had no profile at all — the feature reported itself on
  while being structurally inert, the same shape as the field-site cache that
  shipped switched off.
* **With `CRATONVM_TIER_PGO=1` the profile exists and is EMPTY at the sites that
  matter**: `nest-virtual java/lang/Object.equals -> profile has no receivers at
  that pc (pcs: [])` for `AssertionUtils.objectsAreEqual`, and no profile at all
  for `AssertEquals.failNotEqual` (the failure path never runs). The
  eager-callee-chain compiles these methods before they execute their virtual
  calls interpreted, so the receiver map never fills.

  That cascades: `objectsAreEqual` cannot be nested because it CONTAINS a
  virtual call with no profile, `failNotEqual` likewise, and
  `AssertionFailureBuilder.assertionFailure()` contains a `new`, which the
  resolver refuses outright. Every static-nesting refusal in the trace reduces
  to one of those.

**So the next lever is a data problem, not an emitter one.** The receiver
evidence exists, in the wrong place: an already-compiled `objectsAreEqual` has a
MIC/PIC slot at its `equals` site holding the class it actually dispatches to.
Reading the callee artifact's inline-cache slot — real runtime evidence,
available exactly where the profile is not — is what would make the guarded
splice fire. The emitter half is done and tested.

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
* [`../jit/osr-refused-for-a-loop-inline-in-main-20260810.md`](../jit/osr-refused-for-a-loop-inline-in-main-20260810.md)
  — the shape this looks like and is not; OSR is entered here
  (`osr_entered` non-zero, `osr_refused_entry=0`, `deopts=0`).
