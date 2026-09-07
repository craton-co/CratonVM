# The `jit_bridge` deopt sinks re-ran a side-effecting body from entry, silently

| | |
|---|---|
| **Status** | **FIXED 2026-09-07** (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON — the same switch as the tier-up sink's fix). Residual measured the same day and found empty across 27 547 traps; see *The residual, measured*. |
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

## The residual, measured

The re-run fallback survives this fix: when the frame genuinely cannot be
rebuilt, these sinks still re-enter the method at bci 0. That was left as one
phrase covering NINE distinct causes, none of them counted — so it could be
described and not sized, and nobody could say whether a workload hits it at
all, or which cause to attack first. `try_resume_trapped_callee` had already
written the lesson down: *"a refusal that cannot be named cannot be counted."*

`DeoptFrameBail` names all nine and counts them, and every decline traces under
`CRATONVM_DBG_DEOPT`:

| | |
|---|---|
| `inlined-caller-chain` | the stash names an inlined chain; `resume_from_ir_deopt` handles those, this builder does not |
| `identity-mismatch` | the frame belongs to a different method |
| `superseded-guard-sentinel` | `bci == u32::MAX` |
| `deopt-verify-failed` | `CRATONVM_DEOPT_VERIFY` found a broken invariant |
| `synchronized-with-virtual-objects` | the method monitor may have been elided under scalar replacement |
| `virtual-object-materialise-failed` | re-materialising the object graph failed |
| `unmappable-local-slot` / `unmappable-stack-slot` | a slot the mapper has no representation for |
| `held-monitor-not-an-object` | a monitor that is not a resolved reference |
| `operand-stack-did-not-fit` | the rebuilt stack overflowed the frame |

**On every workload measured, the residual is empty:**

| workload | optimizing bodies | traps TAKEN | frames that could not be rebuilt |
|---|---:|---:|---:|
| the witness (`DeoptRerunProbe`) | 1 | 1 | **0** |
| `CriteriaWindowFunctionTest` | — | 1 | **0** |
| `ASTParserLoadingTest` | — | 1 972 | **0** |
| `ASTParserLoadingTest`, `CRATONVM_C2_ACCEPT=always` | 1 859 | **27 547** | **0** |

That last row is the point: forcing the acceptance gate to keep every
optimizing body it builds produces 1 859 of them and **27 547 traps taken at
runtime**, and every one of them was resumed precisely. The residual is not
"rare on the workloads we tried" — it did not occur once in 27 547 opportunities
on the most trap-dense arm available.

### It is reported, not just counted

A non-zero count is a **silent wrong answer**, so it is NOT behind a debug flag:
`report_unrebuildable_frames` warns at exit whenever the total is non-zero,
naming the reasons. A clean run prints nothing. The zero-confirmation line stays
behind `CRATONVM_DBG_JITC`, so a diagnostic run can tell "the residual is empty"
apart from "the census is not wired up" — which are otherwise the same silence.

Everything else in `interp_census.rs` is a diagnostic you switch on because you
are already looking. This one is not that, and gating it would have left the
residual exactly as invisible as the defect was.

### And it is asserted

`vm/tests/jit_bridge_sink_resumes_instead_of_rerunning.rs` now asserts
`deopt_frame_bail_total() == 0` beside the delta, so a future change that starts
declining rebuilds fails the test rather than silently re-running methods.

It also asserts `lambda_site_deopt_outcomes().1 == 0`. That accessor's own doc
called `unresumable` *"the metric a regression test asserts is zero"* and it had
**no caller anywhere in the tree**; this is that caller, and it covers the third
sink (`resume_deopted_body`), which the fixture does not otherwise reach.

## Field validation, one binary and one switch

`ASTParserLoadingTest` on the fixed binary, default config against
`CRATONVM_JIT_DEOPT_SINK_RESUME=0`:

| arm | result |
|---|---|
| default | **106/106** |
| `..._RESUME=0` | 104 ok / 2 failed — and 0 started in a second run, dead during setup |

The OFF arm's failures are the abort, on ordinary Hibernate ORM code:

```
InternalError: ... EntityEntryImpl.isNullifiable ... at bci 46
  (can_deopt_resume=false (no deopt points, or an elided monitor)
   and CRATONVM_JIT_DEOPT_SINK_RESUME=0, ... reason TransferToInterpreter);
  refusing side-effecting replay
InternalError: ... SessionImpl.instantiate ... at bci 12    [same shape]
```

**Read this arm precisely.** The switch governs BOTH 2026-09-07 sink fixes, and
those two messages are the TIER-UP sink's, so what this A/B demonstrates is that
the tier-up fix is load-bearing for this class on real application code. The
`jit_bridge` half is carried by its own evidence: the witness (delta 2 → 1) and
the 27 547-trap census above. One switch cannot separate them, and this page
does not pretend it does.

Which arm loses which tests also varies between runs (104/2 once, 0 started
once) — the trap only fires once a given method reaches the optimizing tier, so
WHICH methods are compiled when the input arrives is timing-sensitive. The
direction does not vary.

### An observation this page does NOT attribute

`ASTParserLoadingTest` measured `ok=101 failed=5` this morning, on a binary
built before any of today's deopt work, with the five failures identical across
two arms (deterministic, not flaky). It is 106/106 now. The intervening `dev`
range contains both of today's sink fixes AND a substantial amount of unrelated
work from other branches, and the OFF arm above does not reproduce the original
five — so this session has no evidence for what cleared them, and claims none.
`jit-warm-groupdata-window-row-collapse-20260906-FIXED.md` names those five as
not-this-defect; that statement stands, and they are no longer failing.

## What is genuinely still open

Nothing measured. The re-run fallback is still reachable in principle — the nine
causes above are real code paths — but it did not fire once across every
workload run here, including the 27 547-trap arm. If it ever does, the warning
line says so and names the cause.

The one asymmetry left is deliberate: where these three sinks cannot rebuild a
frame they re-run, whereas the tier-up sink refuses loudly. Making them
consistent would mean adding an abort to three paths that have never had one —
a new way for a working workload to die, to fix something with zero measured
occurrences. Not worth it on this evidence; revisit if the warning ever fires.

## Related

* `deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` — the
  fourth sink, and where `CRATONVM_JIT_DEOPT_SINK_RESUME` and the three
  refusals come from.
* `hib-reactive-3gc-run-regressions-FIXED-20260824.md` §3.1 — the field
  instance of this failure mode.
* `inline-trap-inside-a-protected-range-FIXED-20260818.md` — the compiler-side
  refusal that avoids needing a resume at all, for its own narrower shape.
