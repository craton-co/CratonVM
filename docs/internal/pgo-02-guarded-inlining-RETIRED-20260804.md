# PGO-02 — retired 2026-08-04: the residuals, and the miscompile hiding in them

Branch `fix/pgo-02-residuals-20260804`, merged to `dev` and pushed. Retires
[`docs/known-issues/c2/archive/pgo-02-guarded-inlining.md`](../feature-designs/c2/archive/pgo-02-guarded-inlining.md)
for good. The living document is
[`docs/feature-designs/profile-guided-inlining.md`](../feature-designs/profile-guided-inlining.md).

PGO-02's *first increment* (guarded monomorphic virtual/interface inlining,
default-off) shipped 2026-08-03 and the brief was archived. What was left was
the eight-item "to reconcile" list in the consolidated design doc's §8. This
lane closed it — and found, on the way in, that the shipped increment was
**wrong code**.

## The headline: a guard that admitted one class and ran another class's body

`plan_inline` produced a guard on the class the *profile* named. The body
behind that guard was resolved from the class the *constant pool* named — the
receiver expression's static type. Those disagree at every site where the
speculated class overrides the declared method, which is the most ordinary
shape in Java:

```java
private static final A ONLY_B = new B();      // static type A, exact class B
public static int callOverride(int x) {
    return ONLY_B.tag(x);                     // invokevirtual A.tag
}
```

`B` overrides `tag`. The compiler emitted `CMP [recv+0], <B's class id>` and
spliced **`A.tag`'s body** behind it. Every receiver passed the guard, and
every call returned the wrong answer.

**Reproduced, not inferred.** With the pre-fix resolution restored:

```
check_override_receiver: callOverride(506) = 507, want 1506 (call #506 of 700)
```

Calls 0–505 are interpreted and correct; the method compiles at the
500-invocation threshold; call 506 is the first compiled one. No crash, no
diagnostic — the wrong number, forever.

The same family of bug is written up on `try_compile_inner`'s statically-bound
direct-call arm, which was restricted to `invokespecial`/`invokestatic` after
H2 miscompiled `VersionedValue.getCurrentValue`. The comment there says
outright that binding a constant-pool callee at a virtual site "calls THAT body
for every receiver, including one whose class overrides the method". The
guarded-inline path did the same thing one arm below it.

Fixed by `InlineRequest::receiver_callee_resolver` — the body a receiver of
exactly a given class id dispatches to, resolved once per guard class.
`InlineRequest::site` (the constant-pool callee) is `None` for a speculative
site and is *never* a fallback: that fallback is the bug. The VM-side
`resolve_receiver_inline_site` starts the JVMS selection walk at the runtime
receiver and fails closed on every shape where that walk could disagree with
real dispatch (details in the design doc §3).

Two things fell out of the fix rather than being aimed at:

* **`invokeinterface` became reachable at all.** An interface's own method
  declaration has no `Code` attribute, so resolving from the constant-pool
  class had been finding nothing to splice at every interface site.
* **The `InlinedCallee` dependency now names the class that owns the spliced
  body**, not a declared supertype that may own no body.

## Why the existing test did not catch it

Every check built its own `Vm`. Only the FIRST one ever compiled anything — the
tiered background compile worker is process-global and did not warm up again —
so every check after `check_guard_hit` ran fully **interpreted**, including the
guard-MISS check the file's own comment calls "the single most safety-critical
check given the risk profile". They asserted correct results, got them from the
interpreter, and proved nothing about the compiled path.

Found with `CRATONVM_DBG_JITC=1`: exactly one `bg-compile` line in a run of six
checks. The file now shares one warm VM, and every check that depends on a
compiled artifact asserts that artifact exists — so the same regression to
"correct, but interpreted" fails loudly instead of passing.

This is the second finding of the lane and arguably the more transferable one:
**a JIT test that does not assert the method compiled is a test of the
interpreter.**

Adding the assertion then produced two more traps worth carrying:

* **Compilation is asynchronous.** A release build runs the 700-call warm-up in
  ~80 ms — routinely faster than the background worker installs the artifact —
  so a single cache read is a race. It passed on Windows/debug and failed on
  Linux/release. `compiled_tally` now polls while continuing to call the
  method, which gives the worker both the trigger and the time.
* **A test that shells out to `target/release/cratonvm` runs whatever binary is
  on disk.** `cargo test -p cratonvm-vm` does not rebuild `-p cratonvm-cli`, so
  `jit_ir_athrow_dispatch` "failed on this branch" purely because the binary
  predated the dev merge that carried its own fix; rebuilding the binary made
  it pass. Worse, that test SKIPS in 0.00s and reports "1 passed" when the
  binary is absent — which is what a first baseline attempt measured.

## A silent no-op in class-load invalidation

`vm_init.rs`'s `load_class` invalidated for the newly loaded class and its
direct superclass. The superclass was obtained as
`c.superclass.map(|s| s.to_string())` — `superclass` is a `ClassId`, whose
`Display` prints the raw `u32`. The "superclass" handed to the name-keyed
`invalidate_for_class_change` was therefore a **decimal number**, matching
nothing. That half of class-load invalidation had never run.

Nothing failed when it broke, which is exactly why it survived: the guarded and
devirtualised code stays **correct** without the eviction (an exact class-id
guard rechecks the receiver; a MIC/PIC re-targets). The only symptom was code
that should have been retired staying resident, paying a guard that now always
misses, with no event to trigger a recompile. The unit test modelling the VM's
reach passed throughout — it modelled the *intended* behaviour with a
name-taking helper, so it could not see that the VM passed a number.

`load_class` now walks the full supertype closure by name, which also closes
the "known coarseness" the design doc had documented (a dependency on `A` was
not reached when `C extends B extends A` loaded).

## The eight §8 items

| # | Item | Outcome |
|---|---|---|
| 1 | Bimorphic guarded splicing | **Done.** Two guards sharing one receiver load, one null check and one dispatch tail; each guard carries its own body. The buffer estimate and frame spill reservation in `x64::driver` count the second body — they were derived from `inline_sites`, which holds only the primary, and an uncounted second body overflows a buffer this backend cannot retry |
| 2 | A guard that deopts instead of falling through | **Still open, now mechanically enforced.** It needs `FrameState::caller` populated by a producer, which is a deopt-metadata lane. `try_emit_inline` now REFUSES any splice whose body published deopt metadata, so the shape cannot land by accident. Tested by injecting the violation |
| 3 | `StableType` reverse index + `on_class_loaded` | **Done** (`on_class_loaded_with_supertypes`), and the gap that motivated it closed on the other channel too (above). Nothing registers `StableType` from the compiler yet — that needs `InvalidationManager` threaded out from behind `jit_realm`'s mutex, and now buys precision rather than correctness |
| 4 | The metrics/JFR harvest | **Was already done** — `CompileRecorder::installed` harvests the whole tally and `to_json` emits it. The doc's claim had only been checked by reading `metrics.rs`. Now asserted end to end against the reports a real guarded compile publishes |
| 5 | The tally undercounts candidates past budget exhaustion | **Done.** Budget-exhausted and resolver-declined sites are tallied (`budget-already-spent`, `callee-unresolved`) instead of vanishing, so the histogram has a denominator |
| 6 | `jit/src/pgo.rs`'s divergent policy | **Done.** `InliningPolicy` and friends deleted; `ReceiverTypeProfile::shape` is a view onto `classify_receiver_shape` with truncation layered on top. Several of its tests asserted a *ten-observation* site was monomorphic — the live policy calls that `Cold`, which is the disagreement in one line |
| 7 | An uncaught exception out of a guard-hit inlined frame | **Done.** `callDivider` raises `ArithmeticException` from inside a spliced body with no handler anywhere, compared against the same call before the method compiled |
| 8 | The single-pass-only reach | **Measured.** See below |

Plus the brief's own verification requirement that "a truncated or saturated
profile reads as megamorphic rather than as its dominant type", which nothing
implemented: `classify_receiver_shape` now detects saturation before any share
arithmetic and refuses. Truncation is unreachable in the live store (it never
caps the type table) and is handled where it *is* reachable, in `pgo.rs`.

## Item 8, measured: the population is shrinking, and the flag is not the only gate

Two findings, both from `regression-suite/perf/guarded-inline-reach.sh` (new)
against `regression-suite/perf/GuardedInlineReachProbe.java` (new — ordinary
virtual and interface dispatch, because CratonBench is arithmetic and answers a
question nobody asked):

**The feature needs two opt-ins.** `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` buys
the lowering; the receiver profile it reads is collected only under
`CRATONVM_TIER_PGO`, also default-off. With only the first flag set: 15
`no-profile-evidence` refusals, zero splices. Anyone measuring this feature
with one flag concludes the lowering is broken.

**With both flags on, 72% of installed bodies are out of reach:**

```
  installed bodies             18
  single-pass                   5   27.8%
  optimizing (IR)              13   72.2%
  single-pass w/ a splice       0
```

and the reason is sharper than a tier split. `CRATONVM_DBG_JITC=1` shows the
probe's dispatching methods compiled *three times each* — C1, then C2, whose
artifact supersedes the C1 one — with their virtual sites reported by the IR
path as `ir-direct-call MISSED …$Op.apply`. The single-pass artifact carrying
the guard is the one that gets replaced.

So guarded inlining's population, in a warm run, is **the methods the
optimizing tier refuses**, and it shrinks every time a `cov-*` lane lands.
Extending the single-pass lowering is work with a shrinking denominator; the
version worth building, if any, is a guarded-inline lowering on the IR path.
That is now a decision with an instrument behind it instead of an open
question.

## The brief's own "How to verify" / "What to refuse" lists

Separate from the §8 residuals, and initially missed by this lane — three of
its five items had no test until they were asked for explicitly.

| The brief said | Status |
|---|---|
| "a guard that never fires must be byte-identical in behaviour to no inline" | Covered behaviourally by `check_guard_miss` (the receiver is switched to three other classes after the guard is baked in). Byte-identity is asserted for the FLAG-OFF path by construction, not measured for a never-firing guard |
| "same exceptions" | `check_uncaught_from_inlined_frame`, against the same call before the method compiled |
| "same stack traces" | **Measured, and it holds.** The captured trace of an `ArithmeticException` out of a spliced `idiv` names one `tag` frame compiled and one interpreted. A floor assertion stops two zeros from agreeing vacuously. One shape, not a general proof — the general guarantee would be `FrameState::caller` |
| "same `finally` execution" | `check_finally_runs_at_a_guard_eligible_site`: a `finally`-bearing callee is never spliced (non-empty exception table), and the side effect fires exactly once per call on both escape routes |
| "a megamorphic site must refuse" | `check_polymorphic` + the policy tests |
| "a truncated or saturated profile reads as megamorphic rather than its dominant type" | `classify_receiver_shape` gained the `Saturated` arm, checked before any share arithmetic. Truncation is unreachable in the live store and handled where it is reachable, in `pgo.rs` |
| "refuse any inline across a monitor while the frame states carry no monitor list" | `check_monitor_bearing_callees_are_refused` — both a `synchronized` method and a `synchronized` block, asserting the refusal CATEGORY so "refused for an unrelated reason" cannot pass |
| "refuse any speculation seeded from a profile read that is not point-in-time consistent" | Holds by construction: `ProfileStore::get_profile` clones all four maps under one per-slot mutex |
| "is the metadata total for an inlined frame chain of depth k?" | **Not answered — made moot.** The brief said "if there is an input it cannot express, that input must be a refusal". Rather than enumerate depth-k chains, `try_emit_inline` refuses any splice that publishes deopt metadata at all, so no inlined frame chain is ever described. That is strictly stronger for the current design and strictly less informative about the metadata itself |

## Verification

* `vm/tests/pgo02_guarded_virtual_inline.rs` — nine checks, one warm VM, every
  compiled-path claim asserted against a real artifact.
* `jit/src/lib.rs::profile_guided_inlining_tests` — refusal ordering, budget
  arithmetic, the two-guard dependency set, receiver-body-not-declared-body,
  no-fallback, saturated-profile refusal.
* `jit/src/x64/tests.rs::inline_publishing_a_deopt_point_is_refused` — the
  injected-violation test for the deopt postcondition.
* `jit/src/deopt.rs` — `StableType` retirement including the grandchild case.
* `cargo check --workspace --all-targets` clean apart from
  `native-collections/tests/gc_relocation_harness.rs`, which does not compile
  on `dev` either and is unrelated.

## Traps worth carrying forward

* **A JIT test that does not assert the method compiled is a test of the
  interpreter.** The tiered background worker is process-global; a second `Vm`
  in the same process may never compile anything.
* **`ClassId` implements `Display` as the raw `u32`.** Any `to_string()` on one
  that ends up in a name-keyed comparison silently matches nothing, and
  invalidation is precisely the subsystem where "matched nothing" has no
  symptom.
* **A guard tells you which class; it does not tell you which body.** Any
  speculation keyed on a runtime class must resolve its target from that class.
* **`vm/src/runtime/resolve/guard.rs` is a ratchet**, not a checklist: the
  interpreter's metadata-table bypass budget only goes down. A new
  `find_method_recursive` needs a design that does without it, or a migration
  that pays for it.
