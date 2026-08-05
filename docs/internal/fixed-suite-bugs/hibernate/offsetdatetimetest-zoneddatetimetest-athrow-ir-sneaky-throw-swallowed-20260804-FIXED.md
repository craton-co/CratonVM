# `OffsetDateTimeTest`/`ZonedDateTimeTest` — a compiled `athrow` failed to throw, swallowing the real exception behind a `NullPointerException` — **FIXED**

**Status:** FIXED and retired 2026-08-04. Filed the same day as
`docs/known-issues/hibernate/offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804.md`.

Two defects, one lane:

1. **The primary defect** — an IR-compiled `athrow` method swallowed its own
   throw. Fixed by `f9e560dc5`, *"an IR-compiled athrow must force
   has_dispatch, or it swallows its throw"*, on `dev` since 2026-08-04.
   Pinned by `vm/tests/jit_ir_athrow_dispatch.rs`.
2. **The residual the original doc flagged and did not own** — the IR tier's
   shared exceptional-exit stub never stamped `jit_set_throw_bci`, so an
   IR-compiled method's `finally` was silently skipped when a dispatched
   callee threw. Fixed here, branch
   `fix/hib-athrow-sneaky-throw-20260804` (`a466f953a`). Pinned by
   `vm/tests/jit_ir_exception_stub_throw_bci.rs`.

Only the FIRST belongs to COV-07's window: `12b8cbdea` gave `athrow` an IR
lowering on 2026-08-04, which is the first day that shape could be reached at
all. The second predates it — reachable since STUB-S8 admitted
exception-table methods to this tier — and was found by following the original
doc's own "not investigated further here" paragraph.

## Symptom, as filed

`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/`, dev tip
`a43a74ded`, real-JDK, JIT on, default flags:

```
org.hibernate.orm.test.type.temporal.OffsetDateTimeTest FAIL found=488 ok=320 failed=15 aborted=153
org.hibernate.orm.test.type.temporal.ZonedDateTimeTest  FAIL found=608 ok=404 failed=28 aborted=176
```

None of the 43 failures was a real temporal assertion. Every one was a
`NullPointerException` out of JUnit Platform's own exception-dispatch
machinery, in one of two shapes:

```
java.lang.NullPointerException: Cannot throw exception because the return value of
  "org.junit.platform.commons.util.ExceptionUtils.throwAsUncheckedException(java.lang.Throwable)" is null
	at org.junit.jupiter.engine.descriptor.JupiterTestDescriptor.invokeExecutionExceptionHandlers(JupiterTestDescriptor.java:119)
```

```
java.lang.NullPointerException
	at org.junit.jupiter.engine.descriptor.TestMethodTestDescriptor.cleanUp(TestMethodTestDescriptor.java:175)
```

Both bottom out in JUnit's sneaky-throw idiom:

```java
public static RuntimeException throwAsUncheckedException(Throwable t) {
    ExceptionUtils.<RuntimeException>throwAs(t);
    return null;                         // unreachable IF throwAs really throws
}
private static <T extends Throwable> void throwAs(Throwable t) throws T {
    throw (T) t;                         // a void method that is nothing but a throw
}
```

When `throwAs` fails to throw, `throwAsUncheckedException` falls through to
`return null`, whichever caller does `throw ...(t);` throws that null, and the
**real exception is gone** — replaced by Java's helpful-NPE. That is why no
genuine per-test failure detail survived for any of the 43.

(javac erases `throw (T) t` to a bare `aload_0; athrow` — there is no
`checkcast` in the emitted body. The original doc's title said "checkcast
erased-T; athrow"; the `checkcast` is not load-bearing and the defect is the
same without it.)

## Defect 1 — the primary: `athrow` on the IR tier took the no-drain fast entry

**Not** the installation/relocation race the original doc named as its leading
hypothesis, and not a defect in `Op::Throw`'s lowering, which is correct.

The single-pass backend has held an invariant since RBC.6: `emitted_athrow`
forces `has_dispatch` (`jit/src/x64/driver.rs`), so **any** method containing
`athrow` is entered through `execute_jit_call`'s dispatch-aware slow path,
never the raw fast path. COV-07 admitted `athrow` to the IR tier without
bringing that arm across.

`jit_throw_exception` stashes the throwable and returns the `i64::MIN`
sentinel, but — unlike every other sentinel producer — does **not** set
`JIT_DEOPT_PENDING`. The dispatch-aware entry does not need it to: that path
drains `sig.exception` and routes it unconditionally. The `!has_dispatch` fast
entry has no such drain — it consults `deopt_signaled`, which is therefore
false — so the stashed exception is dropped and the method returns as though
it had completed normally. For a `void` method that is invisible: no return
value whose bits could look wrong, just a throw that did not happen.

The fix (`f9e560dc5`) is the missing arm, in `jit/src/lib.rs`:

```rust
if graph.nodes.iter().any(|n| matches!(n.op, ir::Op::Throw)) {
    compiled.has_dispatch = true;
}
```

### Why it fired only once per run

The gap governs exactly one edge: **compiled callee → interpreted caller**.
Once the caller is also compiled, its own JIT-to-JIT sentinel check handles the
i64::MIN and the bug is masked. In a plain hot loop that leaves only the brief
window where `throwAs` is compiled and `throwAsUnchecked` is not yet — which is
precisely the "~600-655 iterations, exactly one occurrence, then never again"
signature the original doc measured and read as a race.

`vm/tests/jit_ir_athrow_dispatch.rs` holds that edge open for the whole run
with `CRATONVM_JIT_DENY`, turning a once-per-run race into a
near-every-iteration certainty: **1 792 397** swallowed throws in 1 800 000
iterations before the fix, **0** after. It also asserts, before anything else,
that `throwAs` was actually reported compiled by the optimizing tier — without
that the green would just be the interpreter (or the single-pass backend, which
was always correct here) doing the work.

### Verification, this session — red → green on the doc's own control binary

`probes/SneakyThrowProbe.java`, `n = 300000`, real-JDK, JIT on, default flags:

| binary | npes |
|---|---|
| HotSpot JDK 25 (control) | 0 |
| `CratonVM-hib-local-0712-v3` @ `a43a74ded` — the binary the doc filed against | **1, 1, 1** (3/3 runs; first NPE at i=4200 / 769 / 792) |
| this branch's binary, dev tip + the residual fix | **0, 0, 0** (3/3) |
| dev tip alone (`41349f661`, before the residual fix) | 0, 0 |

The last row matters: the primary defect was already closed on `dev` before
this branch existed. This session re-derived the red on the exact binary the
doc names, so the transition is measured rather than assumed.

## Defect 2 — the residual: the IR exception stub did not stamp its throw bci

COV-07's closeout doc
(`docs/internal/cov-07-athrow-RETIRED-20260804.md`, *"What this lane does NOT
own"*) recorded this as **confirmed real by inspection, never independently
reproduced**, and flagged it as a follow-up lane. It reproduces.

`JitSignals::athrow_bci` is consumed by `execute_jit_call` as the compiled
method's own throw site and range-tested against `[start_pc, end_pc)` of every
entry in that method's exception table (`find_jit_exception_handler`,
`vm/src/runtime/interpreter/exception_dispatch.rs`). A **dispatched callee**'s
throw travels through the general `set_jit_pending_exception`, which resets
that field to `-1`; only a local `athrow` (`jit_throw_exception`) ever sets a
real bci. So the compiled caller has to stamp its own invoke bci on the way
out. The single-pass backend does, per distinct throw-site bci
(`emit_exception_check_stub`, `jit/src/x64/deopt_stubs.rs`). `ir_lower.rs`'s
`emit_call_exc_stub` emitted ONE shared stub and stamped nothing.

With `throw_pc == usize::MAX`, `find_jit_exception_handler` honours a catch-all
(`catch_type == 0`, i.e. a javac `finally`) **only** when its protected region
spans the whole method — which a `finally` region never does. Typed handlers
still match on exception class, so an ordinary `catch (X e)` was unaffected;
`finally` is the one shape a pc-unknown search deliberately skips. This is the
exact class of bug `FinallyBalanceProbe.java` pinned for the single-pass
backend — same failure mode, different tier.

### The witness

`probes/IrFinallyBciProbe.java`:

```java
static void thrower(int i) { throw new RuntimeException("x" + i); }

static void body(int i, int[] n) {
    try { n[0] = n[0] + 1; thrower(i); } finally { n[0] = n[0] - 1; }
}
```

`body`'s exception table is `from 0 to 12 target 23 any` over a 35-byte method,
so `start_pc == 0 && end_pc >= code_len` is false — the shape the unknown-pc
rule refuses. The invoke sits at bci 9, inside the region.

| arm | `leaked` (skipped `finally` bodies), n = 200 000 |
|---|---|
| HotSpot JDK 25 | 0 |
| CratonVM `--nojit` | 0 |
| dev tip (`41349f661`), JIT on, `body` confirmed IR-compiled | **198 927** |
| this branch, JIT on, `body` confirmed IR-compiled | **0, 0, 0** (3/3) |

Three constraints in the probe are load-bearing, and each was found by watching
the method fall off the tier: no `putstatic` (`0xb3` has no IR lowering), no
`n[0]++` (compiles to `dup2`, `0x5c`, also unlowered), and a `finally` handler
that reads only a **parameter** local (otherwise RBC.6's
`local_handler_reads_unsafe_local` sends the method back to the single-pass
backend, where the defect does not exist and the probe would pass vacuously).
`vm/tests/jit_ir_exception_stub_throw_bci.rs` asserts `body` was reported
compiled by the optimizing tier before it asserts anything about `leaked`.

### The fix

`jit/src/ir_lower.rs`, mirroring `emit_exception_check_stub`:

* `call_exc_patches` becomes `Vec<(patch_offset, throw_bci)>`.
* A new `cur_bci` field tracks the bytecode pc of the node being lowered, set
  at the top of `lower_data_node` (both the MIR and per-opcode routes) and
  `lower_terminator`.
* Every one of the twelve sites that branches to the stub now goes through
  `push_call_exc_patch`, which tags it with `cur_bci` — so no exceptional exit
  can reach the stub untagged, now or when a future fallible op is added.
* `emit_call_exc_stub` emits **one stub per distinct bci**: stamp
  `set_throw_bci(bci)`, reload `RAX` with the sentinel (the helper call
  clobbers it), then the shared epilogue.

`Op::Throw`'s own exit already passes its bci to `jit_throw_exception`, which
stamps it; the stub re-stamps the same value, which is a no-op for that arm and
keeps the stub's contract uniform — every exit through it leaves an
`athrow_bci` belonging to *this* method.

Ordering is preserved for the one consumer that wants the *callee's* bci:
`emit_inline_callee_deopt_service` (which probes the callee's own exception
table) runs at the call site, before the sentinel check branches to the stub.

## Not the moving-young mechanism

Both classes still emit `[moving-young] fallback` WARN lines — moving young
collections are requested and always fall back to the non-moving sweep, exactly
as recorded in
`moving-young-inert-under-jit-throughput-tax-20260730-RETIRED.md`'s final
verification. That was never the cause here: neither class hangs or hits an
internal timeout, both reach `@@RESULT`, and every failure was one of the NPEs
above rather than a `TimeoutException`. GC fallback behaviour is orthogonal.

## Suite verification

`org.hibernate.orm.test.type.temporal.OffsetDateTimeTest` and
`ZonedDateTimeTest`, single-class `CratonRunner` forks on this branch's binary,
real-JDK, JIT on, default flags — the same invocation `run-hib.sh` uses per
class, with no wall cap:

### `OffsetDateTimeTest`

| binary | ok | failed | aborted | `throwAsUncheckedException` hits | wall |
|---|---:|---:|---:|---:|---:|
| `a43a74ded` — the binary the doc filed against | 324 | **15** | 149 | **28** | 596s |
| this branch | 324 | **0** | 164 | **0** | 645s |

`found=488 started=488 skipped=0` in both. `ok` is *identical*; the 15
failures moved into `aborted` (149 → 164), which is the mechanism restated:
`writeThenNativeRead`/`nativeWriteThenRead` self-skip with a JUnit
`TestAbortedException`, JUnit rethrows it through
`throwAsUncheckedException`, and swallowing that throw converted a benign
ABORTED into a FAILED-with-NPE. All 15 baseline failures are those two
methods (14 + 1).

Both arms emit **8** `[moving-young] fallback` WARN lines — identical, which
is the orthogonality claim measured rather than asserted.

### `ZonedDateTimeTest`

Three arms, quiet host, strictly sequential. The third arm is dev tip — the
primary fix WITHOUT this branch's residual fix — because it is the only
comparison that isolates the residual fix's own effect.

| binary | ok | failed | aborted | `throwAsUncheckedException` hits | wall | the remaining failures |
|---|---:|---:|---:|---:|---:|---|
| `a43a74ded` — the binary the doc filed against | 403 | **27** | 178 | **52** | 897s | 27 × sneaky-throw NPE |
| dev tip (`41349f661`) — primary fix only | 403 | 2 | 203 | 0 | 1180s | 1 × 120s `TimeoutException`, 1 × GC-reclaimed-receiver `NoSuchMethodError` |
| this branch — + the residual fix | 403 | **1** | 204 | **0** | 1175s | 1 × 120s `TimeoutException` |

`found=608 started=608 skipped=0` in all three. Same shape as
`OffsetDateTimeTest`: `ok` identical at 403, the fixed failures move into
`aborted` (178 → 203/204), `throwAsUncheckedException` gone. `moving-young`
WARN lines: 12 / 11 / 12 — unchanged across arms.

**The residual `failed=1` is not this bug and not this branch.** It is JUnit's
own per-test 120s cap, and dev tip hits it too — which is what rules out this
branch as its cause.

Do NOT reach for "the host got slower" as the explanation: measured on the
doc's *own* binary (`a43a74ded`), wall time swings in BOTH directions between
the doc's 4-shard run and a single-class fork here —
`ZonedDateTimeTest` 476s → 897s, but `OffsetDateTimeTest` 859s → 596s. Wall
time on this workload is simply high-variance across run configurations, in
both directions, on identical code. A slow test crossing a 120s cap sits
inside that variance. It fired on a different method each time
(`writeThenNativeRead` / `nativeWriteThenRead`), so it reads as a slow test
rather than a hang. Worth a re-time on a quiet host if it recurs; it is not a
temporal or an athrow defect, and it is not what this doc tracked.

**The residual fix costs no measurable wall time**: 1180s (dev tip) vs 1175s
(this branch). The `a43a74ded` arm's 897s is *faster* precisely because 27
tests short-circuited instantly on a swallowed exception instead of running to
completion — which is why the wall number moves in the "wrong" direction on a
fix. Do not read it as a regression.

Dev tip's second failure is a `NoSuchMethodError: java.lang.Object.stream()`
behind `receiver points into RECLAIMED memory — a still-referenced object was
collected` — the separately-tracked, still-open old-gen/young corruption
family, not this doc.

## Test suites

* `cargo test --release -p cratonvm-jit --no-fail-fast` — **1920 passed, 1
  failed** in the lib target; every integration target green (7 / 12 / 14 / 6 /
  10 / 10 / 9 / 21 / 15 / 141 / 3), including `ir_vs_singlepass`, which holds
  `ir_direct_call_exception_sentinel_bails` — the test that exercises the stub
  this branch changed.
  The one failure, `ir_lower::tests::a_wide_field_read_refuses_without_the_
  sentinel_disambiguator`, is **pre-existing red on `dev`**: baselined by
  checking out `HEAD~1`'s `ir_lower.rs` and re-running the single test, which
  fails identically (`L: the sentinel peek must be emitted iff the width can
  collide`).
* `cargo test --release -p cratonvm-vm --test jit_ir_athrow_dispatch --test
  jit_ir_exception_stub_throw_bci` — both green.

  The new test is **proven non-vacuous**, not merely green: re-running its
  compiled test binary with `CRATONVM_BIN` pointed at the pre-fix build
  (`cratonvm-sneaky-base-20260804.exe`, dev tip) fails with `leaked=199144`,
  and its anti-vacuity assertion — `body` reported compiled by the optimizing
  tier — is satisfied on BOTH arms, so the red is the defect and not a
  missed compile.

## The original doc's three "next steps", answered

* **Bisect `a9241eedf..a43a74ded` against `SneakyThrowProbe`** — superseded.
  The mechanism is identified and fixed, and `12b8cbdea` is confirmed as the
  window's opening by construction: `has_athrow` unconditionally refused IR
  admission before it, so `Op::Throw` (and therefore the missing `has_dispatch`
  arm) could not be reached at all.
* **Instrument the bail stub for a relocation/publication race** — the
  hypothesis was wrong. There is no race; the compiled body threw correctly and
  returned the sentinel correctly, and the entry path simply never drained it.
  The once-per-run signature is the compiled-callee → interpreted-caller window
  closing, not a race window.
* **Shift the tier threshold to test the timing hypothesis** —
  superseded by a stronger control: `CRATONVM_JIT_DENY` pins the caller
  interpreted, which holds the governing edge open for the whole run instead of
  merely moving it.
