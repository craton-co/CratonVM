# `HttpHeaderValidationUtilTest` — the two exhaustive loops compile now; a caught exception still costs 2 900 ns

**Status: OPEN, throughput.** The blocker this page led with is CLOSED: both
exhaustive `@Test` loops ran entirely interpreted because the OSR door refused
any method with an exception table, and that refusal (`RBC.6b`) was lifted
2026-08-17 — see the internal record
`fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`.
The class still exceeds the 180 s wall.

Re-measured 2026-08-17 on `perf/osr-exception-table-and-nesting-inline-20260817`,
**Azure Linux host** (the earlier numbers on this page came from a Windows host
and do not transfer — see [The control, re-taken](#the-control-re-taken)),
release build, G1, real-JDK mode, against HotSpot 25 on the same host.

## The control, re-taken

Every budget this page used to carry was derived from a Windows-host HotSpot run
and from a sampling probe calibrated against it. On the Azure Linux host the
real class is much faster than either said:

| | found | started | ok | wall |
|---|---:|---:|---:|---:|
| **HotSpot 25, whole class** | 5506 | 5506 | 5506 | **30.587 s** |
| CratonVM G1, 2026-08-16 and still | 0 | 0 | 0 | **HANG, rc=124 @ 180 s** |

So the 180 s per-class wall allows CratonVM **5.9x HotSpot** on this class — not
the ~2.06x this page previously computed from a 87.4 s figure, and not the ~69x
its sibling gets. Over 8 589 934 592 iterations, 180 s is still
**21 ns/iteration**; what changed is that HotSpot does it in **3.6 ns/iteration**
here, not 8-9.

**`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java` is not
calibrated on this host and its absolute numbers must not be used.** It
extrapolates the class to 476 s under HotSpot — 15x the 30.6 s the class actually
takes. Its window sampling was tuned against the Windows-host distribution, and
the throw rate it hits here is far above the real 7.7%. Read it only as a
same-host ratio between two CratonVM binaries, and use `CratonRunner` on the real
class for anything else. (That is the second instrument on this family of pages
to have manufactured a result; the sibling page carries the note about
`DecomposeProbe`'s 43 ns baseline.)

## What the lift bought, and what it did not

`HeaderValidationLoopRate`, same host, same `n`, two binaries, interleaved:

| | value loop | name loop |
|---|---:|---:|
| dev (`RBC.6b` in force — the loops are interpreted) | 26 361 ns/iter | 8 638 ns/iter |
| this branch (the loops compile) | 14 410 ns/iter | 10 606 ns/iter |

Take the ratio, not the absolute: ~1.8x on the value loop, and the name loop is
inside the run-to-run spread of a host that was running four other release builds
throughout. The class still hangs at 180 s.

That is a far smaller win than "an interpreted loop four to five orders of
magnitude too slow" implies, and the reason is the finding below.

## The finding: a caught exception costs one OSR round trip

The compiled body cannot enter its own handler. Every caught exception therefore
**leaves compiled code entirely** — reason-9 deopt, exceptional-frame
reconstruction, handler search, in-place transfer into the live interpreter
frame — runs one interpreted iteration, and re-enters the artifact at the next
hot back edge.

Counted, not inferred. On `HeaderValidationLoopRate` at `n=1e6` the
`[cratonvm] OSR lifecycle:` line reports `osr_entered=1080047` beside
`osr_exception_handler_entered=1080038`: one OSR round trip per catch.

`probes/OsrExcRateProbe.java` prices it. Five identical once-invoked loop bodies
differing only in throw rate, so the rate=0 arm is the control and
`(t(rate) - t(0)) x rate` is the per-throw cost:

| throw rate | HotSpot ns/iter | CratonVM ns/iter | CratonVM ns per throw |
|---|---:|---:|---:|
| 0 (control) | 1.88 | 71.91 | — |
| 1/64 | 3.77 | 83.08 | **715** |
| 1/8 | 3.91 | 400.28 | **2 627** |
| 1/1 | 8.60 | 2 994.77 | **2 923** |

HotSpot pays 6.7-16 ns. **This page's previous estimate of ~600 ns per
throw/catch, from `probes/ThrowCostProbe.java`, understated it by ~5x** — that
probe's `arm` is invoked once per rep and so is method-entry compiled, a
different tier with a different exception route, and it has no rate sweep to
separate an expensive throw from a slow loop.

At the real 7.7% throw rate, 2 900 ns per throw is **223 ns/iteration on its
own** — ten times the entire 21 ns budget. The throw path, not the call chain, is
the largest single item on this class.

`perf record` on the throw-every-iteration arm says that cost is **flat**, not
one target: interpreted field resolution for the one interpreted iteration each
catch costs (`resolve_field_ref_loader_aware` 5.0%, `load_class_concurrent_for`
3.4%, `is_class_initialized_via_manager` 2.5%, the class-manager read lock 2.4%,
`hash_one::<&str>` plus sip `write` 3.7%, `memcmp` 2.4%), the OSR entry machinery
(`try_osr_with_backoff` 3.7%, `osr_exit_policy` 3.3%,
`route_osr_exception_out_of_artifact` 2.6%, `validate_osr_entry` 1.2%), and 8.6%
in `mi_malloc`/`mi_free`. There is no 10x lever in that list.

## What is left

1. **Compiled exception handlers** — enter the handler without leaving compiled
   code. That is the whole of the 2 900 ns above and it is the largest item. The
   obstacle is not the exception table (the OSR compile now stages it) but the
   emitter's operand-stack model: the single-pass backend walks bytecode
   linearly, so at `handler_pc` its simulated stack is whatever fell through, not
   the JVMS `[exception]`. Nothing in this VM runs a handler in compiled code
   today — the method-entry path re-enters the interpreter at the handler too
   (`route_jit_exception_through_method`) — so this is not OSR-specific and would
   pay off well beyond this class.
2. ~~Two cheap, measured items on the OSR round trip.~~ **DONE 2026-08-17.**
   `osr_exit_policy` was recomputed on every entry though it is a pure function
   of the artifact (3.3% of the profile); it is memoised on the artifact now.
   `try_osr` allocated three `String`s and three `Arc<str>`s per entry attempt
   (part of the 8.6% in the allocator); the frame already held all three as
   `Arc<str>`, so those are refcount bumps now. Worth **~8% at a 1/8 throw rate
   and ~6% at 1/1** on `OsrExcRateProbe`, interleaved, two rounds, both agreeing
   in direction — which is about what the profile predicted, and is also the
   ceiling on this kind of work. The remaining round-trip cost is item (1).
3. **21 ns/iteration** still needs the nesting inliner the sibling page is about,
   and the floor is now measured rather than assumed. `probes/CallArgCostProbe.java`
   (Azure host, deltas over its own no-call control): a compiled static call is
   **4.13 ns**, one taking a reference **6.46**, a virtual one **8.19-8.96** —
   against HotSpot's ~0, because HotSpot inlines all of them. Roughly ten call
   frames therefore cost 40-80 ns before any of them does any work, and the whole
   budget for the iteration is 21. No arrangement of real calls fits; not making
   the calls is the only lever. The sibling page carries the sequenced blocker.
   Steps 1-3 of it landed 2026-08-18 (multi-frame deopt resume, a multi-frame
   OSR-exit transfer with admission relaxed to match, and the single-pass scope
   stack); step 4 — a real call inside a spliced body — is where the remaining
   work is, and its substantive half is resolving the callee's own invoke
   targets against the CALLEE's constant pool, since `InlineSite` has never
   carried any. Its first two steps were VM work rather than compiler work: an artifact
   carrying an inlined caller scope cannot be OSR-entered at all
   (`osr_exit_policy` refuses `caller.is_some()`, because the in-place OSR-exit
   transfer is single-frame), and both these classes' hot methods are `@Test`
   bodies for which OSR is the only door. So multi-frame resume and a multi-frame
   OSR transfer come before inline scopes, calls inside spliced bodies, and
   nesting — same work for both classes.

An honest reading is that this class remains the furthest from reach of the three
`codec-http` walls — which is what the 2026-08-17 revision concluded — but the
reason has moved again. It is not "the loops never compile" (fixed), and it is
not mainly the call chain; it is that every thirteenth iteration leaves compiled
code and comes back.

## Repro

```bash
cd apps/netty-suite-runner
timeout 1500 java @common.args CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

```bash
timeout 1500 cratonvm --java-home <jdk> -Xmx1500m @common.args CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

```bash
cratonvm --java-home <jdk> -cp . OsrExcRateProbe 2000000
```

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 1000000 2>&1 | grep 'OSR lifecycle'
```

```bash
CRATONVM_JIT_OSR_EXC_TABLE=0 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 65536
```

## What was ruled out

Unchanged from the 2026-08-17 revision, and still worth not repeating:

* **The `ByteBuffer.putInt(0, i)` native floor.** Thin direct helpers for
  `Buffer.session()` and `ScopedMemoryAccess.{put,get}IntUnaligned` were written
  and reverted, because the census says they never bind: those JDK accessors take
  the optimizing (IR) pipeline, whose direct-call lowering is register-only
  (`emit_direct_cross_call` requires `num_args + needs_context <= 4` on Windows),
  so a 6-argument `putIntUnaligned` cannot be bound there at all. With the helpers
  on and off, `--dump-native-registry` reports the identical census. Anyone
  picking this up should start by giving the IR path stack-arg marshalling, not by
  writing more helpers.
* **The 120 s JUnit method timeout not firing.** `common.args` sets
  `-Djunit.jupiter.execution.timeout.default=120s`, and Jupiter's default
  `SAME_THREAD` mode cannot preempt a synchronous non-interruption-checking loop.
  Not a separate defect.

## Related

* `fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`
  — what this page's blocker turned out to be, and the fix. It also records the
  two vacuous greens the fix passed through, both caught by an engagement counter
  and neither visible in any correctness result.
* [`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`](httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
  — the sibling. Genuinely a different problem: no `try` anywhere, so it compiled
  all along and its residual is the non-nesting inliner.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the native-call floor this class's `ByteBuffer.putInt` pays once per
  iteration.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — the same family of finding, with per-component throughput measurements.
