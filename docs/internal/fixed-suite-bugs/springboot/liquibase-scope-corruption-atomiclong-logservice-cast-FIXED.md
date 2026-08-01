# Liquibase `Scope` corruption (`Cannot end scope …` / `AtomicLong cannot be cast to LogService`) — FIXED

**Status: FIXED 2026-08-01** (branch `fix/liquibase-scope-20260801`, worktree
`C:\craton\CratonVM-liquibase-scope-20260801`).

Both classes this bug was ever reported against are green under JIT:

| Module | Class | Before | After |
|---|---|---|---|
| `module/spring-boot-liquibase` | `LiquibaseAutoConfigurationTests` | **27 of 43 FAIL**, 5/5 runs, deterministic | **43/43 PASS**, 8/8 runs |
| `module/spring-boot-hibernate` | `HibernateJpaAutoConfigurationTests` | 1 of 70 FAIL (`testLiquibasePlusValidation`, 2026-07-31) | **70/70 PASS** (0 failed, 3 skipped) |

`--nojit` was 43/43 throughout, on the same binary and the same day — the
residual was entirely a JIT defect.

## What the residual actually was

The two shapes the original report named — the scope-id stack mismatch and the
`AtomicLong`/`LogService` cast — **did not reproduce** on `dev 5e4b50b8d`. Their
stale-reference mechanism was closed by the earlier GC work the report's own
"Resolution" sections describe. What was still failing on that class was a
different, deterministic defect that happens to break the same seven test
methods (plus twenty more), and it is worth stating plainly that the original
symptom text was no longer the signature by 2026-08-01:

```
org.springframework.boot.context.properties.bind.BindException:
  Failed to bind properties under 'spring.liquibase.drop-first' to boolean
Caused by: java.lang.NullPointerException:
  Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null
    at org.springframework.boot.context.properties.bind.BindConverter.convert(BindConverter.java:108)
```

### `BindConverter.convert` — the shape

```java
private Object convert(Object source, TypeDescriptor sourceType, TypeDescriptor targetType) {
    ConversionException failure = null;                 // local 4
    for (ConversionService delegate : this.delegates) { // local 5 = the Iterator
        try {
            if (delegate.canConvert(sourceType, targetType)) {   // bci 40, 0xb9
                return delegate.convert(source, sourceType, targetType); // bci 53, 0xb9
            }
        }
        catch (ConversionException ex) {                // handler at bci 62, reads local 4
            if (failure == null && ex instanceof ConversionFailedException) {
                failure = ex;
            }
        }
    }                                                   // back edge: bci 81 -> 14
    ...
}
```

The handler reads `failure`, a **non-parameter** local. That is the population
`local_handler_reads_unsafe_local` (RBC.6) exists to keep out of the JIT — and
that `precise_handler_frames_enabled` has admitted since 2026-07-27 **on the
promise that every throwing site inside a protected range publishes a precise
reason-9 exceptional frame**, so the interpreter can rebuild the real locals
when it resumes the handler.

### The bug

`vm/src/runtime/interpreter.rs` has two sinks that resume a compiled body at its
own handler. Only one kept that promise.

* `route_jit_signal_exception` — consumes the stashed reason-9 frame. Correct.
* `run_jit_callee_handler` — the sink for an exception escaping a compiled
  **callee** that a compiled **caller** dispatched (`jit_invoke_dispatch` →
  `route_implicit_exc_through_callee` → `try_run_callee_handler`). It rebuilt
  the handler frame from `incoming_args` — `this` plus the declared parameters —
  and **never looked at the stashed frame at all**.

Its own doc comment said why that was sound:

> Locals are the callee's incoming arguments … sound for exactly the same
> reason: a compiled method whose handler reads a local first assigned inside
> the try never passes the `local_handler_reads_unsafe_local` compile gate.

True when written. False from the moment the relaxation began admitting exactly
that population. Nothing failed loudly when the premise died: the method still
compiled, the handler still ran, and every local past the parameters simply came
back `0`/null.

For `BindConverter.convert` the parameters are locals 0–3, so `failure` (4), the
**`Iterator` (5)**, `delegate` (6) and `ex` (7) were all zeroed. The handler
itself does not read the iterator — the **loop head it falls through to** does.
So the next `hasNext()` NPE'd on `<local5>`, deterministically, on every bind
whose `canConvert` threw. Under `--nojit` nothing compiles and the class is
clean.

## Evidence chain

1. `CRATONVM_JIT_DENY=BindConverter.convert` → 43/43. Narrowed to that method.
   (`BindConverter.canConvert`, same class, same loop shape: still 27 failures —
   so it is `convert` specifically.)
2. `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES=1` → 43/43;
   `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1` → 43/43. Both refuse the method
   compilation the precise-frame promise enables. Points at that machinery.
3. Ruled out, each with a lever that demonstrably changes codegen or GC and
   left the count at exactly 27: `CRATONVM_JIT_OSR=0`,
   `CRATONVM_JIT_OSR_DEAD_LOCALS=0`, `CRATONVM_NO_MOVING_YOUNG=1`,
   `CRATONVM_JIT_GETFIELD_HELPER=1`.
4. `CRATONVM_DBG_EXCFRAME=1` (extended in this change to print the whole
   published locals vector and the owning method, not just the dropped slots):
   the reason-9 frame at bci 40 is **correct** —
   `[Undefined, RegisterRef(6), RegisterRef(3), RegisterRef(15), RegisterRef(14), RegisterRef(13), RegisterRef(12), Undefined]`,
   with local 5 in r13. The metadata was never the problem; the consumer was.
5. `CRATONVM_DBG_RBC6=1`: `local_handler_reads_unsafe_local=true` for the method
   (so it only compiled under the relaxation), and **28 ×
   `run_jit_callee_handler`** for it — the sink that ignores the frame.
   `route_jit_signal_exception` never names it.

## The fix

`vm/src/runtime/interpreter.rs`:

* `run_jit_callee_handler` now claims the stashed reason-9 frame when it names
  this method (`precise_handler_frame_for`), using its bci as the throw pc and
  its reconstructed locals as the handler frame's locals — the same two-tier
  choice `route_jit_signal_exception` makes. A frame naming a *different* method
  is **re-stashed**, not dropped: unlike the outermost drain, this sink runs
  while the compiled caller is still on the stack and will drain later.
* It **fails closed**: if no matching frame is available and the method's
  handlers read a non-parameter local, it declines the resume rather than
  inventing zeros, leaving the caller's existing conservative behaviour
  (propagate, or the whole-method re-run) intact.
* `cratonvm_jit::handler_resume_requires_precise_locals` exports the RBC.6
  predicate so the sink can tell the two populations apart instead of assuming
  the compile gate keeps the `true` ones out.

`CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1` restores the old behaviour in
full so one binary can be A/B'd against itself.

## Regression test

`vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java` gains a fourth shape,
`loopStep`, where the local at risk is the loop's own iterator and the handler
never touches it. `test_compiled_callee_handler_resume_keeps_the_loop_iterator`
asserts 0 mismatches.

Same binary, both arms:

| | `loopMismatches` (cargo harness / release exe) |
|---|---|
| `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1` | **19497 / 19481 of 20000** |
| default | **0 / 0** |

Two things in that fixture are load-bearing, and both cost a false pass before
they were found:

* **`loopCall`.** `run_jit_callee_handler` is only entered when a *compiled*
  caller dispatched the throwing callee. `loopMismatches` runs its loop once and
  is OSR-denied, so calling `loopStep` from it directly left the exception on the
  ordinary drain — 0 mismatches in both arms.
* **No `instanceof` inside the `try`.** The first draft tested
  `step instanceof Thrower` there. `instanceof` is 0xc1, which
  `precise_frame_publishing_opcode` does not admit, so the whole method was
  refused compilation — again 0 in both arms.

A `probes/`-style standalone probe was attempted first and abandoned: the body
under test never reached the tiering counter (a monomorphic `invokevirtual` call
site from an interpreted `main` does not tick it), so it passed 400,000
iterations while the defect was wide open.

## Test results

* `LiquibaseAutoConfigurationTests`: 43/43 × 8 runs (JIT); 43/43 (`--nojit`).
* `HibernateJpaAutoConfigurationTests`: 70 started, 0 failed, 3 skipped (JIT).
* `cargo test -p cratonvm-vm --test jit_local_exception_handler_tests`: 16/16.
* `cargo test -p cratonvm-jit --lib`: 1226 passed, 0 failed.
* `cargo test -p cratonvm-types --lib`: 477 passed, 0 failed.
* `cargo test -p cratonvm-vm --lib`: 2335 passed, 0 failed. (The harness process
  then exits 1 with no reported failure — **pre-existing**, reproduced
  identically with this change stashed.)
* `cargo test -p cratonvm-vm --tests --no-fail-fast`: 6 failures, all
  **pre-existing** and reproduced identically with this change stashed —
  `class_loader_unload_regression` (2) and `jdk_only_config` (4). Everything
  else green.

## What this does not close

The suite runner's 2026-07-31 rerun showed 18 FAIL / 20 HANG / 3 CRASH across 49
classes. Only the two classes above were this bug; the rest are unrelated and
still open.
