# A deopt inside an INLINED callee stashes a frame nobody can attribute

**Status:** OPEN (2026-08-10). Found while closing
`gc-variant-fullsuite-crashes-hangs-fails-20260810` (now retired to
`docs/internal/fixed-suite-bugs/h2-suite-bugs/`). The *symptom* it caused there
— `org.h2.test.scripts.TestScript` and `org.h2.test.synth.TestCrashAPI` dying
with a fatal `InternalError` under all three collectors — is fixed. The
mechanism underneath is not.

## What happens

`jit/src/deopt.rs`'s `LAST_DEOPT` is a thread-local holding the frame a
compiled artifact reconstructed when it trapped. Consumers attribute it by its
baked `method_key`.

When the trap is inside a callee that the JIT **inlined**, the stashed frame is
keyed to the *inlinee* and its `caller_frames` is **empty**. There is then no
call site on the dispatch path that can claim it: the compiled method that
actually owns the machine frame is the *caller*, and the caller's own identity
is nowhere in the stash.

Traced with `CRATONVM_DBG_DEOPT=1` on `TestScript`:

```
[cratonvm-deopt] callee-resume refused (stash is not this call site's callee):
    stash=org/h2/util/StringUtils.cache:(Ljava/lang/String;)Ljava/lang/String;
    site=get(Ljava/lang/String;)...
```

`org.h2.value.ValueVarchar.get(String)` calls `StringUtils.cache(String)`; the
JIT inlined the latter, the trap landed in the inlined body, and the frame came
out keyed `StringUtils.cache` with zero caller frames.
`try_resume_trapped_callee` (vm/src/jit/helpers.rs) compares the key against the
call site's callee — `get` — declines, and by its own documented contract leaves
the frame stashed "so it propagates to the outer consumer that CAN attribute
it."

**For an inlinee that consumer does not exist.** The frame is orphaned in the
thread-local.

## Why an orphan is not harmless

1. It poisons `has_last_deopt()`, which `jit_dispatch_threw` uses to tell a
   genuine deopt sentinel apart from a method legitimately returning
   `Long.MIN_VALUE`. An orphan makes an unrelated later call site read a real
   return value as a deopt.
2. It is taken by whichever sink runs next, which then has to decide what to do
   with someone else's frame. Until 2026-08-10 the first-call tier-up sink
   (`vm/src/runtime/interpreter.rs`, `execute-first-call-tierup`) answered that
   with a fatal `InternalError`:

```
precise deoptimization unavailable for
org/h2/expression/function/StringFunction1.getValue(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;
at bci 54 (the stashed frame belongs to a different method, stashed key
"org/h2/util/StringUtils.cache:(Ljava/lang/String;)Ljava/lang/String;",
inline callers 0, reason UnreachedCode); refusing side-effecting replay
```

   which killed `TestScript` at ~212 s and `TestCrashAPI` at ~211 s, in every
   GC variant. That sink now does what its sibling `jit-callsite-b`
   (`vm/src/runtime/interpreter/jit_bridge.rs`) already did for the same case:
   de-speculate the frame's real owner, drop the orphan, and let the innocent
   method fall through to interpreted execution. **That is symptom relief, not
   the fix** — it makes the orphan survivable, it does not stop one being made.

## The actual fix, not yet done

A trap inside an inlined callee must reconstruct a frame **chain**: the inlinee
plus its inline caller(s), outermost key naming the compiled method that owns
the machine frame. The machinery already exists — `ReconstructedFrame` has a
`caller_frames` vector and `jit/src/deopt.rs` populates it from per-caller
states (see the `method_key: caller.method_key.clone()` loop) — so the question
is why it comes out empty here: whether the x64 backend does not emit caller
states for this deopt point, or emits them and something drops them before the
stash.

Answering that is a smaller, decidable question than the symptom:

1. Reproduce with `CRATONVM_DBG_DEOPT=1` on
   `org.h2.test.scripts.TestScript` (~3 min, deterministic across five
   observed runs) and find the deopt point the JIT recorded for the inlined
   `StringUtils.cache` body.
2. Compare the caller-state count the *compiler* recorded at that point with
   the `caller_frames.len()` the *runtime* reconstructed. If the compiler
   recorded none, the gap is in emission; if it recorded some, the gap is in
   reconstruction.
3. Once the chain is present, `try_resume_trapped_callee` should match on the
   OUTERMOST key rather than the innermost, which is what makes `get`'s call
   site able to claim its own frame.

## Related

- `docs/internal/fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md`
  — the sweep this came out of, and the sink fix.
- `docs/internal/fixed-suite-bugs/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`
  — where the identity gate and `despeculate_stashed_frame_method` came from.
  Its "outer consumer that CAN attribute it" assumption is exactly the one an
  inlinee breaks.
