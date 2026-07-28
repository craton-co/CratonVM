# JIT/OSR: an exception from a compiled callee made the OSR'd loop re-run iterations — FIXED

**Status: FIXED 2026-07-28.** Retired from `docs/known-issues/`. Regression
coverage: the four `test_osr_loop_does_not_rerun_iterations_*` tests in
`vm/tests/jit_local_exception_handler_tests.rs`, driving the committed fixture
`vm/tests/resources/cratonvm/JitOsrLoopProgress.java`.

Five defects were found behind the one reported symptom. The first two are the
reported bug; the other three are a SIGSEGV the same fixture exposed, each a
distinct way for a JIT code buffer to be unmapped while it is still reachable.

## Symptom

A loop running in OSR-compiled code called a method that threw. The exception
surfaced at the OSR return, `try_osr` took the "safe reject" path (`return None`
= "OSR rejected, keep interpreting THIS frame from where it was"), and the loop
iterations the OSR'd code had already committed were **executed again** — the
interpreter frame's induction variable and accumulators were never updated by the
OSR'd code.

```
cratonvm/Count3.run()   20,000 iterations, callee throws on 1 in 3
  real JDK 25            iters=20000  checksum=280463264
  cratonvm (any config)  iters=20008  checksum=-206531626
```

The 8 extra iterations decompose exactly: five OSR entries (at i = 2000, 3000,
5000, 9000, 17000), each re-running the iterations between OSR entry and the next
throwing call — 2 + 1 + 2 + 1 + 2.

That was the mild form. When the exception *escapes* the OSR'd method rather than
being swallowed downstream, the reject resumes the loop with no exception in
flight at all, so it runs to completion and throws only on the second pass:
**12,346 iterations requested, 42,730 executed.**

## Defect 1 — the OSR tier baked a direct CALL into a callee that declares handlers

`execute`'s method-entry `callee_compiler` closure has had a BUG-H gate since the
`TestHexUtils`/`HexUtils.getDec` fix: never bake a direct machine-code `CALL` into
a callee with a non-empty exception table, because the direct `CALL` bypasses
`jit_invoke_dispatch` and therefore `route_implicit_exc_through_callee` — the only
place that re-runs such a callee in the interpreter so its own `catch` executes.

`compile_osr_artifact`'s eager invokestatic callee wiring (the OSR tier's
independent copy of that logic) never had the gate. OSR'd
`JitOsrLoopProgress.runCaught` therefore direct-called compiled `step`, whose
`catch (Boom)` could never run, and the Boom it was supposed to swallow surfaced
at the OSR return five times per 20,000-iteration run.

Fixed with `osr_callee_declares_handlers` and a matching clause in
`compile_osr_artifact`, mirroring the method-entry gate: loader-aware resolution
via the caller's `ClassId`, and unresolvable metadata reports "has handlers",
which keeps the always-correct dispatch helper.

## Defect 2 — `try_osr`'s exceptional drains discarded committed loop progress

All four drains — pending exception, pending NPE, pending AIOOBE, pending
`ArithmeticException` — ended in "re-stash the signal and `return None`". The
comment on that path was explicit that a safe reject is "correct only when the
bail precedes any committed loop iteration", which is true for the
unconditional-at-header OSR-exit trigger it was written for and false here: the
exception can surface at any invoke, arbitrarily far into the loop.

The key observation the original write-up missed is that **an OSR'd method
provably cannot catch anything**: `compile_osr_artifact` refuses to OSR a method
with a non-empty exception table (RBC.6b) and refuses one that `athrow`s
(RBC.6). So an exception reaching the OSR return *always* escapes the OSR'd
method, and the correct action is simply to propagate it out of the frame. No
precise exceptional frame at the throwing invoke is needed — neither candidate fix
listed in the original doc was required.

Fixed with:

* `OsrBackoffOutcome::ThrowJava(ObjectRef)`, a new outcome variant;
* a `throw_out: &mut Option<ObjectRef>` out-parameter on `try_osr` (its return
  type has no error arm, and widening it would touch every `return None` in a
  ~500-line function);
* `propagate_osr_exception`, which re-checks the empty-exception-table invariant
  rather than assuming it, falling back to the historical re-stash if a future
  gate relaxation makes it false;
* all fourteen dispatch-loop call sites handing the throwable to the
  interpreter's existing `pending_java_exception` channel, which searches this
  frame's handlers and then unwinds — the same machinery `athrow` uses.

The throw is checked *before* `record_osr_rejection`: the OSR'd body ran and
committed work, so it is not a rejected attempt and must not consume the per-pc
rejection budget.

## Defect 3 — the OSR tier never rooted its baked direct-call targets

Found because the fixture also SIGSEGV'd on ~1–2 % of runs idle and ~20 % under
load, with `pc == addr` at a page-aligned address — a jump into an unmapped code
buffer. `CRATONVM_DBG_JIT_UNMAP=1` (added in `ExecutableBuffer::drop` by this
change) plus `CRATONVM_DBG=jitc` named it in one run:

```
full-compile   JitOsrLoopProgress.leaf(I)I        entry=0x74ae18b23000
OSR-compile    JitOsrLoopProgress.runEscape(I)I   entry=0x74ae18b21000   <- bakes CALL 0x…b23000
full-compile   JitOsrLoopProgress.leaf(I)I        entry=0x74ae18b1f000   <- tier-up replaces leaf
[jit-unmap]    ptr=0x74ae18b23000                                        <- old leaf unmapped
OSR-reuse      JitOsrLoopProgress.runEscape(I)I   entry=0x74ae18b21000
SIGSEGV at pc=0x74ae18b23000, addr=0x74ae18b23000
```

`compile_osr_artifact` sets `_jit_strings`, `_jit_invoke_infos`, `_jit_mic_slots`
and `_jit_pic_slots` on the artifact, but never `_direct_callee_entries` — so
`JitCache::prepare_for_publication` had nothing to root and the OSR body's baked
CALLs were kept alive by nothing. `baked_callee_pins` covers only the emit window,
exactly as its comment says. The method-entry path has populated
`_direct_callee_entries` all along (`jit/src/lib.rs`).

Fixed by collecting `osr_direct_callee_entries` in the eager-callee loop and
handing them to `put_osr`. Only real compiled-artifact entries are recorded: the
intrinsic/helper direct calls emitted alongside (`Math.sqrt`, `Integer.valueOf`,
`Integer.intValue`, the `HashMap` fast paths) are Rust function addresses with no
owning artifact.

Measured interleaved under identical load: **14/100 → 0/100**.

## Defect 4 — publication proceeded with an already-retired direct-call target

The remaining ~1/300 SIGSEGV had a different trace: `step`'s C2 compile baked a
CALL to `maybeThrow`'s live entry, a concurrent `maybeThrow` recompile replaced
and unmapped that body *before* `step` reached publication, and `step` was
published anyway with the dangling CALL.

`prepare_for_publication` already detects this ("Returns `false` when a baked
direct-call target could not be pinned, in which case this body must NOT be
published"), but the enforcement was behind `CRATONVM_JIT_STRICT_CALLEE_ROOTS`,
default-OFF since the diagnostic that discovered the family (`e4b848394`). The
default path merely counted the event and published.

Flipped to default-ON. Refusing publication costs one wasted compile — the method
stays interpreted and is recompiled on a later invocation, by which point the
callee has a live body. `CRATONVM_JIT_STRICT_CALLEE_ROOTS=0` restores the
historical behaviour for bisection.

## Defect 5 — a superseded artifact was unmapped while a thread was executing in it

The last residual (~1/600 under load) had yet another trace: the `[jit-unmap]`
line for a page, then a SIGSEGV **mid-body** in that same page — offset 0x553 of
`maybeThrow`'s 1732-byte body, not its entry. Defects 3 and 4 both fault at a
callee's *entry*; this one faults wherever the running thread happened to be.

`defer_jit_owner` / `ACTIVE_JIT_EXECUTIONS` already exist for exactly this: a
process-wide "is any thread inside JIT code" counter, with retirement queued
until it reaches zero. Only the inline caches used it. Every `JitCache` mutation
— `put`, `put_osr`, `invalidate_entries`, `clear_all` — dropped the artifact it
replaced synchronously, so `ExecutableBuffer::drop` unmapped the code under a
running mutator. A background tier-up publishing a C2 body for a method a thread
is currently executing is not a corner case; it is the normal tiering event.

All four sites now hand the replaced artifact to `defer_jit_owner`, which drops
it immediately when no JIT execution is in flight (the common case, so retention
is unchanged) and otherwise holds it until quiescence.

## Reproduction

`vm/tests/resources/cratonvm/JitOsrLoopProgress.java`, four shapes, each returning
a MISMATCH COUNT (0 == pass) rather than a checksum — a checksum-shaped fixture
cannot tell "wrong value" from "extra iterations", whereas a per-iteration visit
tally re-checks instead of corrupting the expected value.

| entry point | shape | before |
|---|---|---|
| `caughtMismatches` | callee catches its own throw | 12 |
| `escapeMismatches` | exception escapes the OSR'd method | 2–10347 |
| `npeMismatches` | implicit NPE from the OSR'd body | 10347 |
| `aioobeMismatches` | implicit AIOOBE from the OSR'd body | 10347 |

All four are 0 after the fix, matching real JDK 25. The doc's original `Count3`
driver now reports `iters=20000 checksum=280463264` — bit-identical to HotSpot.

Shape requirements, all three load-bearing:

* the loop must be in a **called** static method — with the loop directly in
  `main` the counters come out right, so any repro that inlines it looks clean;
* that method must have no exception table (which is also what OSR requires);
* the loop body must commit a per-iteration side effect *before* the throwing
  operation, or the re-run is invisible.

## Interaction with the precise-handler-frame gate

`precise_handler_frames_enabled` (default since 2026-07-28) widens the population
of *compiled* methods that throw and catch internally, so it widened exposure —
but did not cause any of these. Measured on the same binary: `iters=20008` with
the gate on and with `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1`, and the same
20008 on an unmodified `dev` build.

## Unrelated pre-existing breakage noticed while verifying

Neither is caused by this change; both reproduce on a pristine `dev` tree.

* `cargo test -p cratonvm-jit` does not compile: five integration tests
  (`intrinsic_arraycopy`, `intrinsic_arrays_sort`, `intrinsic_long_bits`,
  `intrinsic_string_access`, `intrinsic_string_search`) construct
  `JitRuntimeHelpers` without its `set_throw_bci` field.
* `cargo test -p cratonvm-jit --lib` SIGSEGVs in
  `x64::tests::live_monitor_ops_execute_direct_runtime_stubs`. Verified by
  reverting this change's `jit/src/lib.rs` edits and rebuilding: it crashes
  identically.
