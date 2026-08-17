# A once-invoked method whose hot loop contains a `try`/`catch` runs entirely interpreted — the OSR door refuses any method with an exception table

**Status: OPEN, reproduced and diagnosed 2026-08-17, not fixed.** The refusal is
deliberate and named (`RBC.6b`); what is not deliberate is its blast radius.
Measured on this host, release build, G1, real-JDK mode: an identical loop runs
**compiled when the `try` is removed and interpreted when it is present**, and
`java.util.function`-shaped test methods pay 19 000-309 000 ns/iteration where
HotSpot pays 8-9.

Found while sizing
[`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](../netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md),
whose two exhaustive `@Test` loops each wrap one call in a `try`/`catch`. That
page had sized the class as a *compiled-code* throughput wall — "the same wall as
the sibling with twice the iterations". It is not: the loops never compile.

## Reproduction

`probes/OsrDenyShapeProbe.java`. Six once-called methods, each with a hot loop,
differing in one feature. `CRATONVM_DBG_JITC=1`, one run:

```
OSR-compile         OsrDenyShapeProbe.plainLoop(I)V      entry_pc=4  len=1082
OSR-compile FAILED  OsrDenyShapeProbe.tryCatchLoop(I)V   osr_bci=4   — OSR-denied for the rest of this process
OSR-compile         OsrDenyShapeProbe.newBeforeLoop(I)V  entry_pc=9  len=1502
OSR-compile         OsrDenyShapeProbe.anonBeforeLoop(I)V entry_pc=13 len=3223
OSR-compile         OsrDenyShapeProbe.doWhileLoop(I)V    entry_pc=4  len=786
OSR-compile FAILED  OsrDenyShapeProbe.tryCatchDoWhile(I)V osr_bci=4  — OSR-denied for the rest of this process
```

`try`/`catch` is the only discriminator. An allocation before the loop, an
anonymous class before the loop, a `do`/`while` instead of a `for` — all
compile. A `try` anywhere in the method does not, and the denial is permanent for
the process.

**There is no `codegen-bail` line**, because codegen never runs. The refusal is a
front-end `return None` in `compile_osr_artifact`, roughly thirty of which are
silent. That is why this took a shape bisect to find rather than a flag:

## The refusal, and its own justification

`vm/src/runtime/interpreter/jit_bridge.rs`, `compile_osr_artifact`:

```rust
// RBC.6b (dohead-residuals, 2026-07-18) — never OSR a method with its own
// local exception handlers, even when it never directly `athrow`s.
// `compile_with_param_slots` below has no exception-table parameter, so an
// OSR artifact NEVER carries handler ranges: a callee exception unwinding
// into this OSR-compiled frame finds no catch and escapes uncaught, even
// though a `catch` block textually guards the call.
if has_exception_handlers {
    mark_jit_bail_listed(&class_name, &method_name, &method_descriptor);
    return None;
}
```

The hazard it prevents is real and was observed: a servlet's
`try { resp.resetBuffer(); } catch (IllegalStateException)` silently stopped
catching once an earlier blanket `ldc`-string OSR denial was lifted. So the
refusal must not simply be deleted.

## Why the cost is so much larger than "this method is a bit slower"

A method invoked ONCE has no other door. The eager first-call tier is opt-in
(`CRATONVM_JIT_C2_FIRST_CALL`), and the invocation-counted tier-up needs
invocations this method will never get, so OSR is the only route out of the
interpreter for its loop — the same structural point
[`osr-refused-for-a-loop-inline-in-main-20260810.md`](osr-refused-for-a-loop-inline-in-main-20260810.md)
makes for a loop inline in `main`. Refuse OSR and the loop is interpreted for its
whole life.

That shape is not exotic. It is what a `@Test` method is, what a `main` is, and
what any one-shot harness/driver method is — and "a hot loop with a `try`/`catch`
in it" is ordinary Java.

Measured on the netty page's two loops
(`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java`, which is the
two `@Test` bodies with bounded sampling):

| | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| `headerValueValidationMustRejectAll...` loop | 9.4 ns/iter | **309 423 ns/iter** | 33 000x |
| `headerNameValidationMustRejectAll...` loop | 8.2 ns/iter | **19 242 ns/iter** | 2 100x |

`CRATONVM_DBG=jit-method-stats` on the same run, 131 072 iterations:
**`deopts=65115 c2_bailouts=65105`**, and
`hot_but_stuck_in_interpreter=3 (ineligible-by-policy=3)`.

## The diagnostic gap that hid it, now closed

The OSR door called `mark_jit_bail_listed`, which records the *fact* of a
permanent bail and not its *reason* — so a method denied here reached the
end-of-run `CRATONVM_DBG=jit-method-stats` table as `reason=unrecorded`, which
is exactly the shape that sends a reader looking for a compiler bug somewhere
else. `jit::try_compile` has always recorded one. The door now calls
`mark_jit_bail_listed_with_site`, which consumes the same thread-local refusal
site and, under `CRATONVM_DBG_JITC`, prints

```
[cratonvm-jitc] OSR-bail site=<site> pc=<pc> opcode=0x.. <class>.<method><descriptor>
```

This is the third time in this file's history that a missing counter — not a
missing measurement — is what made a compile door's behaviour unfalsifiable; see
the `Thread.currentThread` and `reachabilityFence` bind comments a few hundred
lines above for the other two.

## What would fix it

The machinery for a compiled frame that carries handler semantics already
exists, and the method-entry door uses it: `jit::try_compile` stages
`set_precise_exception_frame_request`, `set_protected_ranges_request` and
`set_pending_exception_ranges`, and `emit_post_invoke_exception_check` then emits
a **reason-9 (`PendingException`) deopt frame** at every invoke inside a
protected range, keyed on the *throwing* bci. `route_jit_signal_exception`
consumes that frame and finds the handler in the method's real exception table.
Nothing in that chain is method-entry-specific.

What is OSR-specific is the exit: a deopt out of an OSR body must transfer state
**in place** into the pre-existing live interpreter frame
(`transfer_osr_exit_into_live_frame`), and that transfer is fail-closed — on
refusal the OSR trigger resumes interpretation at the STALE pre-OSR back-edge
pc. For an exception exit that fallback is not merely slow, it is the
silent-corruption shape `RBC.7` documents: iterations already committed by the
OSR'd code get re-executed.

So the fix is not "stage the three requests and delete the refusal". It is:

1. stage the three requests for the OSR compile, so protected-range invokes emit
   reason-9 frames;
2. make the stale-resume fallback **unreachable for an exception exit** rather
   than merely unlikely — the describability of a deopt point's slots is decided
   at compile time (`FrameValue::Unsupported` comes from the slot classifier), so
   the OSR compile can verify at publish time that every reason-9 frame at a
   protected-range pc is fully describable and single-frame, and refuse the
   artifact otherwise. That turns a runtime reject into a compile-time refusal,
   which is the only form that is safe here;
3. only then lift `RBC.6b`, gated, with the servlet `resetBuffer` shape as a
   regression test.

Until (2) exists, lifting the refusal trades a throughput bug for a silent
wrong-answer bug, which is the wrong direction.

## Repro

```bash
cd apps/netty-suite-runner
javac -nowarn -d . ../../probes/OsrDenyShapeProbe.java
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> -cp . OsrDenyShapeProbe 300000 \
  2>&1 | grep -E 'OSR-compile( FAILED)?'
```

## Related

* [`osr-refused-for-a-loop-inline-in-main-20260810.md`](osr-refused-for-a-loop-inline-in-main-20260810.md)
  — the same "OSR is the only door for a once-invoked method" structure, refused
  for a different reason (an unresumable deopt point) and with a named refusal
  the tooling could already print.
* [`../netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](../netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — the workload this was found from, and the page whose sizing it corrects.
