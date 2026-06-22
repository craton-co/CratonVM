# JIT `idiv`/`irem` divide-by-zero re-runs the whole method (side effects double-execute)

**Status:** ✅ FIXED on `dev` (branch `fix/jit-divzero-direct-throw`, ported from
`feat/coupled-deopt-moving-spine` commit `1fccf659`). JIT integer-division zero guards now
direct-throw `ArithmeticException` through the method's exception table instead of re-running
the whole method. Verified vs HotSpot + bt18 soak (see **Verification** below). This file is
retained in `docs/internal/` as the historical record per the known-issues triage rule.

**Regression test:** `cargo test -p cratonvm-vm --features synthetic-jdk --test exception_tests`
— `test_jit_{idiv,irem,ldiv,lrem}_zero_no_double_side_effect` (drive the divide method hot so it
JIT-compiles, then assert the single trapping call's pre-divide side effect runs exactly once)
plus `test_jit_divzero_throws_catchable_arithmetic`. The original `test_nested_try_catch` stays
green but runs interpreted (a single top-level `vm.invoke` does not tier up), so it did not
actually exercise the JIT path — the new tests do.

## Symptom (historical)
`cratonvm/ExceptionAdvanced.testNestedTryCatch` should print `[1, 2, 3]`; the buggy JIT printed
`[1, 1, 2, 3]` — the **first** side effect executed twice.

```
assertion `left == right` failed
  left: [1, 1, 2, 3]
 right: [1, 2, 3]
```

## Bytecode (the relevant shape)
```
 0: iconst_1
 1: invokestatic cratonvm/Util.tempPrint(1)   // prints 1   <-- duplicated
 4: iconst_1
 5: iconst_0
 6: idiv                                        // 1/0 -> ArithmeticException
...
Exception table: from 4 to 8 target 11 (ArithmeticException)
```

## Root cause
The method is JIT-compiled. On the `idiv` divide-by-zero the JIT took an **uncommon-trap /
deopt that re-executes the whole method from the interpreter** (`jit_uncommon_trap` →
whole-method re-run). Every side effect *before* the trap (here `tempPrint(1)` at pc 1)
therefore ran a second time. HotSpot throws directly with no re-run (delta = 1); CratonVM's
re-run gave delta = 2.

Confirming evidence:
- Only div-by-zero exception tests failed this way; non-arithmetic exception tests pass — so it
  was specific to the `idiv`/`irem` path, not general exception dispatch.
- The interpreter `idiv` path is correct (re-pushes operands, falls to the slow path, throws
  `ArithmeticException` via the exception table with no re-run). The bug was JIT-only.
- Same class of bug as the AIOOBE "uncommon-trap re-run" pattern — except AIOOBE was already
  correct (delta = 1) because it direct-throws via `jit_throw_aioobe`. That asymmetry was the
  smoking gun.

## Fix (as landed)
Mirror the JIT's existing **direct-throw** path for AIOOBE (`jit_throw_aioobe`):
- `jit-api`: new `RequiredPtr` helper slot `throw_arithmetic`.
- `vm/src/jit/helpers.rs`: `jit_throw_arithmetic()` sets a pending-arithmetic flag + the deopt
  signal and returns the `i64::MIN` sentinel (no method re-run); `take`/`stash` accessors.
- `jit/src/x64.rs` `emit_deopt_stubs`: special-case deopt `reason == 3` (DIV_BY_ZERO) → CALL
  `throw_arithmetic` + epilogue instead of the `uncommon_trap` re-run stub. Checked *before* the
  deopt-osr frame-deopt trampoline so reason 3 always direct-throws regardless of the
  `CRATONVM_DEOPT_REAL` gate.
- `vm/src/runtime/interpreter.rs`: drain the pending-arithmetic flag at all four JIT-return
  sites (early-dispatch, OSR bail, `execute_jit_call`, `execute_jit_call_decoded`) and throw
  `ArithmeticException("/ by zero")` through the method's own exception table.

The uncaught stack trace omits the JIT frame — pre-existing and identical to AIOOBE direct-throw
(inherent to direct-throw: no frame reconstruction).

### Porting note (dev reconciliation)
Cherry-picking `1fccf659` onto `dev` conflicted in `jit/src/x64.rs` with the deopt-osr
frame-deopt trampoline (reasons 2/7); resolved by placing the `reason == 3` direct-throw check
first. `dev` had also independently added the `dispatch_threw` helper field, so the `jit-api`
field-count literals were reconciled (`NUM_FIELDS` 42→44, required-ptr 35→37) and the new field
added to the ten `jit/tests/*.rs` helper fixtures.

## Verification
- **Real-JDK CLI differential** (`scratch/divzero/`): `idiv`/`irem`/`ldiv`/`lrem` side-effect
  delta 2→1; nested-try-catch `1 1 2 3`→`1 2 3`; `ArithmeticException` message `/ by zero`; all
  == HotSpot. The buggy path was reproduced behind a temporary env gate to confirm the
  differential had teeth (delta 2, `1 1 2 3`).
- **bt18 soak** (`binarytrees 18`): checksum `68332206`, ~34s, no regression.
- jit-api 29 + jit 825 lib + jit integration + 11 exception unit tests green.

## Note on the synthetic-jdk harness
Under `--features synthetic-jdk` the JIT-exercising unit tests use a heap-array side-effect
probe (`c[0]++`), not a static counter: a JIT'd writer's static-field write was not observed by
an interpreted reader in that mode (a separate synthetic-jdk quirk, orthogonal to this fix), and
synthetic `ArithmeticException.getMessage()` does not carry the HotSpot `/ by zero` string
(the interpreter path returns the "wrong message" verdict too). The exact message text is
therefore asserted only in the real-JDK CLI differential.
