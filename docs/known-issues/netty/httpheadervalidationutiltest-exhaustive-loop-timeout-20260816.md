# `HttpHeaderValidationUtilTest` — the deopt storm is fixed; what is left is the per-iteration wall

**Status: OPEN, throughput.** The defect this page has named as "the whole gap"
since 2026-08-20 — 1.67 `ReceiverTypeChanged` deopts per loop iteration ending
in `MakeNotCompilable` — is **CLOSED 2026-08-25**. It was not the bimorphic
receiver this page said it was, and the fix is worth **6.6x on the value loop
and 2.6x on the name loop**, with the class's deopt census going from
6 236–123 832 entries to **19**. See
`fixed-suite-bugs/jit/string-receiver-guard-speculated-with-no-evidence-FIXED-20260825.md`.

**The class still does not finish.** With the fix it exceeds a **5 400 s** cap,
against the harness's 180 s wall and HotSpot's 53 s. Everything specific to this
class is now closed or measured dead; what remains is the generic per-iteration
cost of compiled code, which is the subject of
[`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
and not of this page. This page stays open because the class is not green, and
it is now a pointer to that wall plus a list of what must not be re-tried.

All numbers below: **Azure Linux host `vm1`**, 2026-08-24/25, release build,
real-JDK mode, one binary with `CRATONVM_JIT_RECEIVER_DESPEC` off and on.
Load is printed with every reading, because this host is shared and a loaded
reading of this class is worthless.

## Where the class stands

| | found | started | ok | wall |
|---|---:|---:|---:|---:|
| HotSpot 25, whole class (load 10.4) | 5506 | 5506 | 5506 | **53.0 s** |
| CratonVM, default, cap 5 400 s (load 23.8) | — | — | — | **no `@@RESULT`** |
| CratonVM, `CRATONVM_JIT_RECEIVER_DESPEC=0`, cap 5 400 s (load 12.7) | — | — | — | **no `@@RESULT`** |
| budget (the 180 s wall) | | | | **180 s** |

The two exhaustive `@Test` loops run 2^32 iterations each, 8 589 934 592 in
total; the 180 s wall is therefore **21 ns/iteration**, and HotSpot does it in
roughly 6.

**`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java` remains
uncalibrated on this host and its ABSOLUTE numbers must not be used** — it hits
a throw rate far above the real 7.7% and over-states the class. Read it only as
a same-host, same-binary ratio between two arms. Its COUNTERS are exact, and
everything decisive below is a counter.

## FIXED: an evidence-free `String` receiver guard at every `CharSequence` site

The previous revision of this page ended: *"1.67 `ReceiverTypeChanged` deopts
per iteration … The receiver at those sites alternates between `AsciiString` and
the `CharSequence` wrapper the test builds … So the speculation is not
mis-tuned; it is **wrong about the program**."*

The last sentence was right and the reasoning under it was wrong, in a way that
mattered. The two named sites —

```
HeaderValidationLoopRate.oldHeaderValueValidationAlgorithm:(Ljava/lang/CharSequence;)V  bci=6
HttpHeaderValidationUtil.validateValidHeaderValue:(Ljava/lang/CharSequence;)I           bci=1
```

— are both `invokeinterface java/lang/CharSequence.length()I`, and the class the
compiler speculates there is **neither** `AsciiString` **nor** the wrapper. It
is `java/lang/String`: `try_resolve_string_intrinsic` inlines the String-layout
decode at any `CharSequence`-declared `length`/`charAt`/`isEmpty` site behind an
exact class-id compare, and the miss edge of that compare is a **deopt**. The
receiver is never a `String` here, so the guard failed on every call — and the
compiler took that bet with no receiver evidence at all, because
`CRATONVM_TIER_PGO` is default-OFF and there is no profile to read.

The fix requires positive evidence (or the de-spec registry's veto) before a
guarded String intrinsic is emitted, asked at the resolver in both compile
doors. Full write-up, the economics that set the 90% bar, and the correctness
gates are on the fix page.

**What it is worth on this class**, one binary, six interleaved readings per
arm, idle host (load 1.1–2.9):

| | guard on (`=0`) | default | ratio |
|---|---:|---:|---:|
| value loop, ns/iter | 4617.59 – 5594.00 | **657.65 – 722.72** | **6.6x** |
| name loop, ns/iter | 2648.46 – 5070.30 | **1153.60 – 1250.41** | **2.6x** |

and the counters, at `n=20000` (65 536 iterations), four readings per arm:

| | guard on (`=0`) | default |
|---|---:|---:|
| deopt-log entries | 6 236 – 123 832 | **19** |
| deopts at `oldHeaderValueValidationAlgorithm` bci 6 | 4 419 – 4 440 | **0** |
| `reason=UnreachedCode` traps | 0 – 36 006 | **0** |
| `receiver despec: guards-emitted` | 146 – 171 | **0** |
| `receiver despec: profile-declined` | 0 | **11** |

### `reason=ReceiverTypeChanged` is not a receiver-guard signature

Worth writing down, because it is what made the previous revision's diagnosis
half-right. `snapshot_pre_intrinsic_call(pc, DeoptReason::ReceiverTypeChanged)`
runs at the top of the intrinsic ladder **and** on the plain MIC/PIC dispatch
arm, so reason 6 is stamped on the pre-invoke snapshot at essentially every
invoke bci. `CRATONVM_DBG_DEOPT=1 | grep 'reason=' | sort | uniq -c` names the
**site** exactly and the **cause** not at all. Read the emitted guard, or the
`receiver despec:` counters, before believing a reason label.

### The de-spec registry lever this page named, and where it had to go

The previous revision's recommendation was: *"make receiver-type speculation
consult the de-spec registry … That is one read in the emitter and a policy arm
in `recommend_action`."* One read in the **emitter** is exactly what it must not
be, and this cost most of a session:

A site the resolver registers as an intrinsic takes
`direct_calls.push(..); continue;` in `try_compile_inner` **above** the
`invoke_info.push`, so it has no dispatch metadata. Dropping the direct call in
`x64::bytecode_walk` therefore leaves that pc with neither `direct` nor
`info_ptr`, and the emitter falls through to the unconditional `UnreachedCode`
trap at the bottom of the invoke arm — the "declined" site deopts on **every**
execution instead of dispatching. Measured: 33 declines at
`oldHeaderValueValidationAlgorithm pc=6`, one compile after the last of them,
and **1 466 deopts at that bci afterwards**, with `unreached=85 193` in a
65 536-iteration run.

The consult now lives at the resolver in both doors. **The two pre-existing
backend filters of the same shape — `ArraycopyPrimitive`'s de-spec filter and
`StringIndexOfChar`'s constant-needle filter — have the identical defect and are
untouched;** each needs its own A/B, and both are named in the comment that
replaced the removed filter.

The policy arm did land: `recommend_action_at_bci` no longer escalates a
`ReceiverTypeChanged` deopt to `MakeNotCompilable` at a bci already in the
de-spec registry, bounded by `CRATONVM_JIT_DESPEC_SPARE_FACTOR` (default 2)
times `max_deopts_per_method`.

## What is left, and it is not this class's

At the fixed rate the two loops still cost far more than the 21 ns/iteration the
wall allows, and the class exceeds a 5 400 s cap. Nothing on this page's list is
still actionable here:

1. ~~**Compiled exception handlers.**~~ DONE 2026-08-20.
2. ~~**Two cheap items on the OSR round trip.**~~ DONE 2026-08-17.
3. ~~**1.67 `ReceiverTypeChanged` deopts per iteration.**~~ **DONE 2026-08-25**,
   above.
4. **The call chain and the per-iteration floor.** Unchanged and now the whole
   remainder. `probes/CallArgCostProbe.java` prices a compiled static call at
   **4.13 ns**, one taking a reference at **6.46**, a virtual one at
   **8.19–8.96**, against HotSpot's ~0. Ten call frames cost 40–80 ns before any
   of them works, and the budget for the whole iteration is 21. This is
   [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)'s
   subject, not this page's.

   **Narrowed 2026-08-26, and measured NOT to help this class.** The 14-store
   full-GPR blind spill `emit_pre_safepoint_spill` emits at every GC-capable
   call is now elided where the caller frame is provably oop-clean
   (`CRATONVM_JIT_CALL_SPILL_ELISION`, default on), which is worth **1.4-2.2x
   on every shape of compiled call** in `CallArgCostProbe`. On THIS class it
   does nothing — three interleaved rounds each, quiet host, `652-685` vs
   `684-718` ns/iter on the value loop — and the counter says why in one line:
   `elided=1 ... ref-local-in-reg=116`, every refusal the same clause. A frame
   that keeps a receiver in a register-homed local cannot use the elision, and
   the next lever is to NARROW the spill to the registers that can hold an oop
   rather than to elide it. See
   [`../perf/per-call-blind-gpr-spill-20260826.md`](../perf/per-call-blind-gpr-spill-20260826.md).

5. **The inline chain does not reach this loop, and cannot.**
   `compile_osr_artifact` hands the backend an EMPTY `inline_sites` map, so an
   OSR artifact splices nothing — ever — and a `@Test` body invoked once has no
   other compiled form. `probes/OsrVsEntryInlineProbe.java` prices wiring the
   planner into the OSR door by running one loop body through both doors in one
   process, and the answer is **no difference** (47.16 vs 47.43, 49.76 vs 53.99,
   46.44 vs 47.23 ns/iter). Do not spend a session on it.

## Repro

```bash
cd apps/netty-suite-runner
timeout 900 java @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

```bash
timeout 5400 cratonvm --java-home <jdk> -Xmx1500m @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

The A/B for the 2026-08-25 fix, one binary:

```bash
cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 2000000
```

```bash
CRATONVM_JIT_RECEIVER_DESPEC=0 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 2000000
```

The counters that prove it engaged — a zero in `profile-declined` means the fix
did nothing on that run, whatever the clock says:

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 20000 2>&1 | grep 'receiver despec'
```

The deopt census:

```bash
CRATONVM_DBG_DEOPT=1 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 20000 2>&1 | grep -c 'cratonvm-deopt'
```

## What was ruled out

* **The `ByteBuffer.putInt(0, i)` native floor.** Thin direct helpers for
  `Buffer.session()` and `ScopedMemoryAccess.{put,get}IntUnaligned` were written
  and reverted, because the census says they never bind: those JDK accessors
  take the optimizing (IR) pipeline, whose direct-call lowering is register-only
  (`emit_direct_cross_call` requires `num_args + needs_context <= 4` on
  Windows), so a 6-argument `putIntUnaligned` cannot be bound there at all.
  Anyone picking this up should start by giving the IR path stack-arg
  marshalling, not by writing more helpers.
* **The 120 s JUnit method timeout not firing.** `common.args` sets
  `-Djunit.jupiter.execution.timeout.default=120s`, and Jupiter's default
  `SAME_THREAD` mode cannot PREEMPT a synchronous non-interruption-checking
  loop. It does, however, REPORT afterwards — see the sibling page, where that
  distinction is the difference between "12 ok" and "13 ok". Either way it is
  not a separate defect here, because this class never reaches the report.
* **Wiring the inline planner into the OSR door**, item 5 above — measured, not
  argued.

## Related

* `fixed-suite-bugs/jit/string-receiver-guard-speculated-with-no-evidence-FIXED-20260825.md`
  — what this page's third blocker turned out to be, the fix, and the
  measurement that sets its 90% evidence bar.
* `fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`
  — this page's first blocker. It also records the two vacuous greens that fix
  passed through, both caught by an engagement counter.
* [`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`](httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
  — the sibling, and the other class in this family that is still short of its
  wall.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — where item 4, this page's entire remainder, belongs.
