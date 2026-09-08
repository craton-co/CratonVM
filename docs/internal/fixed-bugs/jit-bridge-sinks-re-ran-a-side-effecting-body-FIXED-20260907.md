# The `jit_bridge` deopt sinks re-ran a side-effecting body from entry, silently

| | |
|---|---|
| **Status** | **FIXED 2026-09-07** (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON — the same switch as the tier-up sink's fix). |
| **Was** | OPEN, filed by reading while fixing the sibling sink, with no witness. |
| **Severity** | A silent wrong answer — a duplicated side effect. Worse than the abort the tier-up sink used to raise, because nothing said it happened. |

## The measurement that closed it

The page this replaces said the fix would be *"a behaviour change to working
code justified by a source reading"*, and refused to make it on that basis.
That was the right bar; here is it cleared.

`probes/DeoptRerunProbe.java` (and its in-repo twin, the
`cratonvm/DeoptRerunCount` fixture): one array store, then a division. The
store is the observable; the division is what the optimizing IR tier lowers to
a **deopt guard** rather than to a throw. One trapping call, counted either
side:

| arm | side effects for ONE call |
|---|---:|
| HotSpot JDK 25 | 1 |
| CratonVM `--nojit` | 1 |
| CratonVM, JIT, C1 body | 1 |
| **CratonVM, JIT, optimizing body** | **2** |
| CratonVM, JIT, optimizing body, `CRATONVM_JIT_DEOPT_SINK_RESUME=0` | **2** |
| **CratonVM, JIT, optimizing body, after this fix** | **1** |

and `CRATONVM_DBG_DEOPT=1` names the sink and the bci exactly (the class was
`RerunDrv` in the scratch copy the numbers were taken from; the committed probe
is the same file under its final name):

```
[cratonvm-deopt] sink=jit-callsite-a running= stash=DeoptRerunProbe.hot:(II)I bci=14
acc=104400 delta=2  (RE-RAN: side effect twice)
```

bci 14 is the `idiv`. The compiled body committed the `iastore` at bci 11,
trapped at 14, and the sink answered by re-entering at bci 0 — so the store ran
again.

## The defect

A deopt sentinel does **not** mean "nothing happened". It means the compiled
body ran up to the trapping bci and stopped. Re-entering from bci 0 re-executes
everything before it.

Three sinks answered it that way. Each attempted a precise resume first, behind
two preconditions that are false on every optimizing-tier artifact in a
production build:

* `ir_deopt_resume_enabled()` (`CRATONVM_IR_DEOPT_RESUME`) is **default OFF**,
  and its own comment justified that with *"no production IR method emits a
  deopt guard yet, so default OFF is inert"* — which stopped being true. The
  IR tier lowers array access, field access and division to deopt guards, and
  plants an unconditional trap at every `invokedynamic` it cannot lower;
* `compiled.can_deopt_resume` is set by `ir_lower` only under
  `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`.

Both false, so both fell through to `Ok(None)` / `CacheMiss` — "re-execute this
method from entry". No side-effect check guarded that fallback, unlike the
tier-up sink's, so the duplication was silent.

| sink | where | reached when |
|---|---|---|
| `execute_jit_call` (`jit-callsite-a`) | `jit_bridge.rs` | the `invokestatic` path |
| `execute_jit_call_decoded` (`jit-callsite-b`) | `jit_bridge.rs` | the virtual/interface path |
| `resume_deopted_body` | `jit_bridge.rs` | both one-shot lambda doors |

## This had already happened once, in the field

`resume_deopted_body`'s own doc states the rule its code could not keep:

> **This is the only correct answer for a body that has already committed a
> side effect.** … Declining — returning `Ok(None)` so the caller re-enters the
> body from entry — therefore re-executes everything before that bci a second
> time.

and names what it cost: the hibernate-reactive `reactiveRemove`-fires-twice
defect (`hib-reactive-3gc-run-regressions-FIXED-20260824.md` §3.1) — one
`ArrayLoop.next()` dispatch, **two deletes**, `--nojit` clean. The second
delete found the row already gone and threw, which aborted `@AfterEach` before
indices 3 and 4 were processed, whose rows then collided with the next test's
re-insert.

The 2026-08-24 fix for that added the `resume_deopted_body` call — behind the
same `can_deopt_resume` gate. **So the fix for a duplicated-side-effect defect
could not fire on the tier that produces the most aggressive code.** That is
the shape this page closes.

## The fix

One predicate, `sink_precise_resume_allowed` (`deopt_resume.rs`), OR-ed beside
each sink's existing `can_deopt_resume` arm — at all three sites here, and at
the tier-up sink in `interpreter.rs`, which now asks the same function instead
of spelling the terms out. Four sinks, one answer.

**Strictly additive at every site.** The old arm is untouched; the new one
carries the three refusals the reconstructed frame genuinely cannot answer (an
`ACC_SYNCHRONIZED` method, a body that takes a monitor at all, a resume bci
past the code). Hanging those on the old arm too would refuse a single-pass
body with an ordinary `synchronized` block that resumes correctly today — a
backend that SET `can_deopt_resume` has already vouched no monitor was elided.

The resume goes through `real_frame_deopt_resume_and_despeculate`, not through
raw `resume_real_ir_deopt`, so the identity gate, the epoch-staleness check and
the de-speculation all still happen. That also means the widened branch
subsumes the `else if` / `else` de-speculation arms it now bypasses: that
function de-speculates the frame's real owner on an identity mismatch and the
method itself otherwise.

### One knock-on, stated because it is not a no-op

`real_frame_deopt_resume_and_despeculate`'s per-bci de-spec carried the comment
*"Inert in production (this sink is deopt-real-only)"*. It is no longer inert —
the three `jit_bridge` sinks now reach it on ordinary runs. That is the
intended direction: a single pathological speculation site gets suppressed on
the next compile and the method stays compiled, instead of the whole method
being given up. The comment now says so.

## Evidence, pinned

| arm | file | asserts |
|---|---|---|
| default | `vm/tests/jit_bridge_sink_resumes_instead_of_rerunning.rs` | delta == 1 |
| `..._RESUME=0` | `vm/tests/jit_bridge_sink_rerun_off_arm.rs` | delta == 2 |

Both drive the fixture through **bytecode**, and both check
`CompiledMethod::used_ir_backend` and panic rather than proceed. Two things
that anti-vacuity check caught, in order, and both are now recorded in the test
headers because the next person will hit them too:

1. **Warming through `vm.invoke` pins the method at C1 forever.** With
   `CRATONVM_JIT_C2_FIRST_CALL` off (the default), `execute`'s first-call path
   compiles single-pass and caches a non-IR body, *"which preempts the
   optimizing IR pipeline on every later path"* — its own words. At C1
   `can_deopt_resume` is set and the defect is invisible.
2. **Even at C2, the acceptance gate throws the optimizing body away.**
   `[ir] acceptance …: REFUSED (evidence: none) -- keeping the single-pass
   body`. That is not the gate being wrong: a method simple enough to be a
   clean one-store/one-trap witness is by construction too simple for an
   optimizing body to earn its keep. The tests set `CRATONVM_C2_ACCEPT=always`,
   because the subject is what the sinks do with an optimizing artifact, not
   which methods deserve one.

## What is still open

The re-run fallback itself. When the frame genuinely cannot be rebuilt —
`build_deopt_frame_inner` returning `None` for an inlined caller chain, an
unmappable slot, a malformed monitor — these sinks still re-run from entry, and
for a side-effecting body that is still a duplicated side effect. The tier-up
sink refuses instead, loudly.

Not made consistent here, deliberately: the tier-up sink has ALWAYS had that
abort, whereas adding one to these three would be a new way for a working
workload to die. The residual is now narrow — the resume has to actually FAIL,
not merely be ungated, which is the difference this fix makes.

For the lambda door it is also already counted: `site_unresumable` in the
lambda-site census line is exactly "a deopted body the direct arm could not
resume, so the generic path re-ran it from entry, side effects and all". What
does NOT exist is a test asserting it is zero — `lambda_site_deopt_outcomes()`,
the accessor written for that and documented as *"the metric a regression test
asserts is zero"*, has no caller anywhere in the tree. Wiring it up is the next
thing to pick up; the two sinks that are not lambda doors have no equivalent
counter at all.

## Related

* `deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` — the
  fourth sink, and where `CRATONVM_JIT_DEOPT_SINK_RESUME` and the three
  refusals come from.
* `hib-reactive-3gc-run-regressions-FIXED-20260824.md` §3.1 — the field
  instance of this failure mode.
* `inline-trap-inside-a-protected-range-FIXED-20260818.md` — the compiler-side
  refusal that avoids needing a resume at all, for its own narrower shape.
