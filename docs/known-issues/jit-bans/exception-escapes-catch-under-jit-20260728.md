# An exception escapes its own `catch` under JIT (two ordinary shapes)

**Status:** OPEN. Found 2026-07-28 by the `C2Handlers` differential probe.
Pre-existing — reproduces with the 2026-07-28 exception-table/C2 fix both
enabled and disabled on the same binary, so it is independent of that change.

## Symptom

`Exception in thread "main" C2Handlers$Boom` — a `Boom` thrown inside a `try`
whose `catch (Boom e)` covers it propagates out of the method instead of being
caught. Not a crash, not a wrong value: the handler is simply skipped.

JIT-only. `--nojit` passes all shapes.

## Reproduction

`C2Handlers.java` (scratchpad probe; twelve `try`/`catch` shapes, each run hot
enough to compile and checked against a handler-free expected value). Ten shapes
pass. Two fail, identically, on every configuration tried:

```java
static int mayThrow(int n) {
    if ((n & 1) == 1) { throw new Boom(); }
    return n * 2;
}

// FAILS
static int paramOnlyFast(int n) {
    try { return mayThrow(n) + 1; }
    catch (Boom e) { return -n; }
}

// FAILS
static int switchAfterHandler(int n) {
    int k;
    try { k = mayThrow(n); }
    catch (Boom e) { k = 3; }
    switch (k) { case 0: return 100; case 2: return 200; case 3: return 300;
                 case 4: return 400; case 6: return 600; default: return 900 + k; }
}
```

Driven by a hot loop over `n = i % 23` (odd `n` throws). Both bodies pass on
HotSpot and under `--nojit`.

Shapes that PASS, for contrast — note how close they are to the failures:

```java
static int localsAcrossTry(int n) {          // passes
    int a = n + 1, b = 0;
    try { b = mayThrow(n); a = a + b; }
    catch (Boom e) { b = 99; }
    return a * 10 + b;
}
static int codeAfterHandler(int n) { ... acc += mayThrow(n); ... }   // passes
```

Also passing: nested try, two handlers on one try, a switch *inside* the try, a
`continue` out of a handler, a loop inside the try, and implicit
NPE/AIOOBE/divide-by-zero inside a protected range.

## Where to start

Almost certainly the same family as the gate documented in
[exception-table-callee-barred-from-inline-cache-20260728.md](exception-table-callee-barred-from-inline-cache-20260728.md):
a compiled frame cannot dispatch to its own handler, so correctness depends on
every call into such a method going through the dispatch helper, which
re-executes it in the interpreter on a pending exception
(`route_implicit_exception_through_callee` → `bail_to_interpreter`,
`vm/src/jit/helpers.rs`). Any path that reaches the callee by a **baked
machine-code `CALL`** instead — a JIT→JIT direct call, or an inline-cache entry
whose gate did not fire — bypasses that interception and the exception escapes.

The MIC/PIC population sites are gated on `mic_callee_has_exception_table`; the
statically-bound sibling is gated in the `callee_compiler` closure in
`interpreter.rs`. `paramOnlyFast` and `switchAfterHandler` are both **static**
callees invoked from a hot static caller, so the statically-bound gate is the
first thing to audit — and the memory note on the sibling tail-call hole
(`emit_epilogue_without_ret` jumping past a covering handler, gated on
`Compiler::pc_is_protected`) is the second.

Useful controls already established:
- `--nojit` → all shapes pass, so it is a compiled-dispatch problem.
- `CRATONVM_JIT_NO_EXC_TABLE_C2=1` → no effect, so it is not tier-related.
- The ten passing shapes bound the search: whatever the trigger is, it is absent
  from `localsAcrossTry`, which differs from `switchAfterHandler` only in what
  follows the `try`.
