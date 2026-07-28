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

---

## RESOLVED 2026-07-28 — but the probe then exposed a worse bug behind it

**The escape itself was fixed by dev `66548471f`** ("run a compiled method's
`finally` when an exception passes through it"), which stamped the throwing
method's own bci over the callee's via `set_throw_bci`. Both shapes named above
(`paramOnlyFast`, `switchAfterHandler`) pass on `075fdfc54`: the exception is
caught by its own handler and the right value is returned.

**Running the FULL twelve-shape probe rather than just those two showed the
escape had been replaced by silent wrong values** — 260,206 failures per 200k
iterations, in the two shapes the original report never flagged because it
stopped at the first two that failed:

| shape | symptom |
|---|---|
| `loopInsideTry` | handler returns `-sum`; got `0` |
| `handlerContinues` | loses the accumulated `sum`; off by the running total |

Both mean the same thing: **non-parameter locals arrive at the handler as 0.**
That is strictly worse than the original bug — an escaping exception is loud,
a wrong `int` is not.

### Root cause (fixed in this commit)

A block-level exception edge is not sufficient. `build_cfg_with_handlers` makes
each handler a successor of every block overlapping its protected range, which
correctly puts the handler's live-in into that block's `live_out`. But both
consumers — the per-pc liveness behind the exceptional-frame snapshot, and the
register allocator's interference graph — then walk *backward through the block*
and apply every def between the throw site and the block end:

```
   29: iload_1        sum pushed on the operand stack
   33: invokestatic   <- THROWS here; handler at 41 does `return -sum`
   37: istore_1       sum redefined  -> kills local 1 walking backward
   41: astore_2 / iload_1 / ineg / ireturn

   live_out(B) = {sum}   (handler edge, correct)
    38 goto     -> {sum}
    37 istore_1 -> {}        <- def kills it
    33 invoke   -> live_at[33] = 0x0
```

On the exception path that `istore_1` never executes; control reaches the
handler with the locals as they stand AT bci 33. So the snapshot recorded `sum`
as `FrameValue::Undefined` (→ 0) and the handler returned `-0`.

Fix: union each covering handler's live-in **per pc**, not per block, in
`regalloc::live_locals_per_pc_inner` and `regalloc::build_interference`
(`handler_live_mask`). The allocator half matters just as much — without it the
snapshot can say "local 1 is live in register r" while the allocator has already
given `r` to something defined inside the try.

Why the existing test did not catch it: `live_locals_per_pc_sees_a_local_only_the_handler_reads`
covers a local *only* the handler reads and never redefines. The redefinition
after the throw site is what defeats the block-level edge.

Verified, same worktree and commit, `git stash`-ed A/B:

| | C2Handlers @200k |
|---|---|
| clean `origin/dev` | 260,206 failures |
| with the fix | **0** |

Also clean: 600k iterations, `--nojit` control, and `HandlerLocals` (the RBC.6
conformance probe, 200k).

### Still open, found on the way

`CallPathProbe` leaks on all five dispatch routes on clean dev, including the
three `66548471f` fixed — see
[finally-skipped-again-callpathprobe-all-five-routes-20260728.md](finally-skipped-again-callpathprobe-all-five-routes-20260728.md).
Independent of everything above (reproduced with this fix stashed).

**Diagnostic added:** `CRATONVM_DBG_EXCFRAME=1` prints every local DROPPED from
an exceptional-frame snapshot — the actionable signal for this whole bug class,
where the failure mode is a handler quietly reading 0/null.
