# An inline trapping bytecode (array bounds check) inside a `try` range has no deopt point — the invoke-site exception machinery doesn't cover it

**Status: OPEN, reproduced and diagnosed 2026-08-17, not fixed.**

Found triaging the Apache Commons Math test suite
(`apps/commons-math/RESULTS-20260817.md`): `SparseRealVectorTest` crashes (not
merely fails) with a hard `InternalError` on every test that touches equality
or serialization of a sparse vector:

```
java.lang.InternalError: JIT dispatch into
  org/apache/commons/math4/legacy/linear/OpenIntToDoubleHashMap$Iterator.advance()V
  failed: internal error: precise deoptimization unavailable for
  org/apache/commons/math4/legacy/linear/OpenIntToDoubleHashMap$Iterator.advance()V
  at bci 51 (can_deopt_resume=false (no deopt points, or an elided monitor),
  stashed key "org/apache/commons/math4/legacy/linear/OpenIntToDoubleHashMap$Iterator.advance:()V",
  inline callers 0, reason TransferToInterpreter); refusing side-effecting replay
	at org.apache.commons.math4.legacy.linear.OpenMapRealVector.equals(OpenMapRealVector.java)
```

## The bytecode shape

`Iterator.advance()` (`OpenIntToDoubleHashMap.java:550`) uses array-bounds
exceptions as loop control — a deliberate, if old-fashioned, idiom: walk a
`states` byte array forward until either a `FULL` marker is found or the index
runs off the end, and treat the resulting `ArrayIndexOutOfBoundsException` as
"no more elements":

```java
public void advance() throws ConcurrentModificationException, NoSuchElementException {
    if (referenceCount != count) throw new ConcurrentModificationException();
    try {
        current = next;                 // side effect #1
        while (this$0.states[++next] != FULL) {}   // side effect #2, then baload
    } catch (ArrayIndexOutOfBoundsException e) {
        next = -2;
        if (current < 0) throw new NoSuchElementException();
    }
}
```

`javap -c` confirms: bci 51 is the `baload` inside the loop, inside an
exception table entry `[30, 56) -> 59 (ArrayIndexOutOfBoundsException)`, and
**two field-store side effects** (`current = next` at bci 22-27, `next =
next+1` at bci 30-37) happen on every loop iteration *before* the trapping
`baload` — including iterations before the one that actually throws.

## Why the refusal is correct, not just unhelpful

`vm/src/runtime/interpreter.rs`'s comment at the refusal site says this
plainly: *"Precise reconstruction is a correctness requirement once native code
has executed past bci 0. Refuse a whole-method replay: it is observably wrong
for methods with stores, I/O, monitor actions, or callbacks."* That is exactly
this method's shape — a naive "re-run `advance()` from bci 0" fallback would
re-read `this.next` (already mutated by however many loop iterations ran in
compiled code before the trap) as the *fresh* starting value, silently
skipping whatever iteration state a real interpreter continuation would have
had. CratonVM correctly detects it cannot safely do this and raises a hard
error instead of guessing — the bug is that it ever compiled this method into
a shape that needs this fallback at all.

## This is the same general bug class already found and half-fixed once

`docs/internal/fixed-bugs/unresumable-unconditional-trap-mvmap-FIXED-20260802.md`
diagnosed the identical error shape (`can_deopt_resume=false (no deopt points,
or an elided monitor)` / `refusing side-effecting replay`) for
`org.h2.mvstore.MVMap.evaluateMemoryForKey`, root-caused there to a floating
`Op::Div` guard getting scheduled above the branch that reaches it. That fix
(`IrBuilder::add_div_zero_guard` anchoring the trap to its real control block)
is specific to the IR optimizing tier's handling of `Op::Div`/`Op::Rem` and
does not touch array bounds checks.

**This is a different producer of the same shape.** `advance()`'s trap is a
`baload` bounds check, compiled via the older/baseline x64 bytecode walker
(`jit/src/x64/bytecode_walk.rs` opcode `0x33`, `emit_bounds_check` in
`jit/src/x64/arrays.rs`) rather than the IR tier's guard machinery. That
compiler's bounds-check stub (`emit_bounds_check_stubs`,
`jit/src/x64/deopt_stubs.rs`) calls `jit_throw_aioobe` directly and — per this
run's evidence — does not register a deopt point for the trap, so
`can_deopt_resume`'s `!cm.deopt_points.is_empty()` half of the gate is false
for this method: **there are no deopt points at all**, not merely a missing
one at bci 51.

## Why the exception-table doesn't already save this

Same-day sibling doc
`docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
describes the machinery that *should* apply here: "the method-entry door...
stages `set_precise_exception_frame_request`, `set_protected_ranges_request`
and `set_pending_exception_ranges`, and `emit_post_invoke_exception_check`
then emits a **reason-9 (`PendingException`) deopt frame** at every invoke
inside a protected range". `advance()` was compiled via the method-entry door,
not OSR — RBC.6b (OSR's blanket refusal of any method with an exception table)
does not apply to it. But the crash's `reason` is **`TransferToInterpreter`**,
not `PendingException` — the reason-9 machinery that doc describes is scoped
to **invoke sites** inside a protected range (a callee method that might
throw). `advance()`'s trap is not a call to another method; it is an **inline
bytecode instruction's own bounds check** inside the protected range. That
shape falls through the invoke-site machinery entirely and lands on the older,
generic `TransferToInterpreter` fallback — which, for this compiler path,
finds no deopt point and refuses rather than corrupt.

**In short:** exception handling for *calls that might throw inside a `try`*
appears to be handled; exception handling for *bytecode instructions that
might throw inside a `try`* (bounds checks, in this case; plausibly also
`athrow`-adjacent null checks and div guards on the baseline walker specifically)
is not, at least not on the baseline x64 walker.

## What would fix it

Given the H2 precedent's explicit warning — *"Do not apply the publish-side
rule blind... the naive form would refuse every trap-carrying artifact,
including the many whose re-run-from-entry fallback works fine"* — and its own
example of a first diagnosis that blamed the wrong mechanism, this needs the
same staged approach that sibling doc lays out for OSR, adapted for
method-entry compiles on the baseline walker:

1. Either extend the reason-9 `PendingException` deopt-frame machinery to
   cover inline trapping instructions (bounds check, null check, div guard)
   inside a protected range on the baseline walker, keyed on the trapping bci
   the same way invoke sites are; or
2. at minimum, make the *compile-time* bail mirror `can_deopt_resume` for this
   shape specifically — a method containing an exception-table range that
   covers a trapping inline instruction with no deopt point, and a side effect
   reachable before that instruction, should fail to compile (falling back to
   the interpreter) rather than publish an artifact guaranteed to crash on its
   first trap. `jit/src/x64/bytecode_walk.rs`'s existing `unresumable_trap`
   check (~line 10324, currently scoped to invokedynamic sites) is the
   established precedent for this kind of compile-time refusal and the
   natural place to generalize from.

Not attempted here — this touches the same deopt-point-emission and
exception-table interaction the sibling OSR doc treats as multi-step,
correctness-critical work, not a quick patch.

## Reproduction

```bash
CV="<worktree>/target/release/cratonvm.exe"
JDK="<jdk25>"
CP="<see apps/commons-math/RESULTS-20260817.md>"
RUNNER="<CratonRunner.java from apps/netty-suite-runner/, compiled standalone>"

"$CV" --java-home "$JDK" --Xmx 1g -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.linear.SparseRealVectorTest
# -> InternalError as above, on testEquals/testSerial/etc.
```

## Related

* `docs/internal/fixed-bugs/unresumable-unconditional-trap-mvmap-FIXED-20260802.md`
  — the H2/`MVMap` instance of the same error shape, different producer
  (`Op::Div` scheduling on the IR tier), already fixed. Its "Lessons worth
  keeping" section (misattributed deopt frames, `reason` defaults, "denying
  the named method is not a diagnosis") applies directly here too.
* `docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
  — the invoke-site exception machinery this bug falls outside of, and the
  staged-fix pattern this doc's "What would fix it" borrows.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
