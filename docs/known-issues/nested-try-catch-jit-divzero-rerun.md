# JIT `idiv`/`irem` divide-by-zero re-runs the whole method (side effects double-execute)

**Status:** OPEN. Root-caused (high confidence). Fix exists on a separate branch; not yet on `dev`.

**Failing test:** `cargo test -p cratonvm-vm --features synthetic-jdk --test exception_edge_tests test_nested_try_catch`

## Symptom
`cratonvm/ExceptionAdvanced.testNestedTryCatch` should print `[1, 2, 3]`; CratonVM prints
`[1, 1, 2, 3]` — the **first** side effect executes twice.

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
The method is JIT-compiled. On the `idiv` divide-by-zero the JIT takes an **uncommon-trap /
deopt that re-executes the whole method from the interpreter** (`jit_uncommon_trap` →
whole-method re-run). Every side effect *before* the trap (here `tempPrint(1)` at pc 1)
therefore runs a second time. HotSpot throws directly with no re-run (delta = 1); CratonVM's
re-run gives delta = 2.

Confirming evidence:
- Only div-by-zero exception tests fail this way; non-arithmetic exception tests
  (`test_basic_exception_catch`, etc.) pass — so it is specific to the `idiv`/`irem` path,
  not general exception dispatch.
- The interpreter `idiv` path is correct (re-pushes operands, falls to the slow path, throws
  `ArithmeticException` via the exception table with no re-run). The bug is JIT-only.
- This is the same class of bug as the AIOOBE / div-by-zero "uncommon-trap re-run" pattern.

## Fix
Mirror the JIT's existing **direct-throw** path for AIOOBE (`jit_throw_aioobe`,
`jit/src/x64.rs`): add a `jit_throw_arithmetic` helper and make the `idiv`/`irem`/`ldiv`/`lrem`
zero-guards throw `ArithmeticException` through the method's exception table **instead of**
`jit_uncommon_trap` (no re-run). Also route the matching interpreter drains.

This fix has already been implemented and validated (incl. `bt18=68332206`, ~39s soak, no
regression) on branch **`feat/coupled-deopt-moving-spine`** (commit `1fccf659`) — see internal
note "JIT div-by-zero direct-throw". Porting it to `dev` is a JIT-codegen change and **must be
re-verified with the bt18 soak** before landing, so it is intentionally not rushed in alongside
the test-suite repairs.

## Risk
Touches JIT integer-division codegen + a new diverging helper + interpreter drains. Medium-high
regression risk; requires the full JIT regression check (bt18) — do as a focused, separately
verified change.
