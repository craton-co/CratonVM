# LOOP-02 — the four whole-compile refusals, measured

**Status:** the first increment (measure) landed 2026-08-03. The second
(narrow the cheapest gate) is **blocked**, and the measurement is what says so.
**Owns:** `jit/src/x64/loop_rewrite.rs` only.

## The finding

`plan_bytecode_loop_xform` refuses the whole compile, before it looks at a
single loop, when any of these hold:

* `deopt_real` is enabled,
* precise exception frames are in use,
* the method contains `invokedynamic`,
* the method has inline sites.

Each refusal is defensible: they name constructs that publish an emitter pc to
the VM as a resume bci through a path the wiring does not translate.

## The measurement

`metrics::LOOP_XFORM_EVENTS` counts all four **independently**, on every compile
that reaches the planner, and is read at exit with `CRATONVM_DBG=jit-method-stats`:

```
[cratonvm] loop-xform admission: loop_xform_compiles=865 loop_xform_deopt_real=865
  loop_xform_precise_exception_frames=7 loop_xform_invokedynamic=65
  loop_xform_inline_sites=34 loop_xform_eligible=0 loop_xform_not_armed=865 …
```

Counting them independently is the whole point. The planner returns on the
first refusal that holds, so a "which one fired" tally would have recorded
`deopt_real` for 100% of compiles and said nothing about the other three — it is
a process-wide default-ON flag, not a property of any method. The four counts
therefore **overlap** and must never be summed.

Spring Boot, `core/spring-boot-autoconfigure`, three test classes, default JIT
configuration, one run each:

| class | compiles | deopt_real | precise exc frames | invokedynamic | inline sites | **eligible** |
|---|---:|---:|---:|---:|---:|---:|
| `AutoConfigurationSorterTests` | 206 | **206 (100%)** | 0 | 3 (1.5%) | 8 (3.9%) | **0** |
| `ConditionalOnClassTests` | 137 | **137 (100%)** | 1 (0.7%) | 4 (2.9%) | 1 (0.7%) | **0** |
| `ConditionalOnPropertyTests` | 865 | **865 (100%)** | 7 (0.8%) | 65 (7.5%) | 34 (3.9%) | **0** |

## What that says, and what it overturns

**This doc's own first increment named the wrong target.** It proposed narrowing
`InlineSitesPresent` first, on the grounds that its problem is table replication
and the replication primitive is already generic. That is still true, and it
would still buy **nothing**: inline sites cost 0.7–3.9% of compiles, and every
one of those compiles is already refused by `deopt_real`. The same goes for
`invokedynamic` (2.9–7.5%) and precise exception frames (0–0.8%). Narrowing all
three would move `eligible` from 0 to 0.

`deopt_real` is not one of four comparable gates. It is the gate, it fires on
every compile, and it is not a property of the code being compiled — which means
the only work in this lane that changes anything is the one
`docs/jit/loop-rewriter-wiring.md` already calls "the largest remaining piece":
translating `DeoptimizationPoint::bci` at `build_and_record_deopt_point` so the
`DeoptRealEnabled` refusal can be retired.

The doc's own guess is worth recording as a miss: it supposed
"`invokedynamic` accounts for 95% of refusals on Spring code". It accounts for
7.5% at most, and the refusal it loses to is one the doc did not weigh.

## And the blocker under that

Turning `deopt_real` off — the only configuration in which the transform can run
at all today — **SIGSEGVs on real code**. See
`docs/known-issues/jit/deopt-real-off-null-entry-sigsegv-20260803.md`; the
minimal reproducer is `probes/IndyDeoptProbe.java`, 45 lines, no framework, and
the fault is a call to address 0 from `try_call_with_context`. It is not the
loop rewriter (nothing arms it in any of those runs) and it is not in this
lane's file.

So the order of work is now:

1. fix the `deopt-real=0` crash (its own lane — the reproducer is cheap);
2. *then* the `DeoptimizationPoint::bci` translation, which retires
   `DeoptRealEnabled`, `PreciseExceptionFrames` and `InvokedynamicPresent`
   together;
3. `InlineSitesPresent` last, and only if it is still measurably in the way.

Steps 2 and 3 are unchanged in kind by this measurement — what changed is their
order and the knowledge that 3 alone is worthless.

## Reading the tally

Ten rows, `CRATONVM_DBG=jit-method-stats`, printed at exit next to the tiering
stats. The counters do not consult `metrics::enabled()`, so a default run shows
real numbers, and they are also in `metrics::summary()` next to the bailout
table (`MetricsSummary::loop_xform`, and the `"loop_xform"` object in its JSON).

| row | meaning |
|---|---|
| `loop_xform_compiles` | denominator: compiles that reached the planner |
| `loop_xform_deopt_real` … `loop_xform_inline_sites` | the four conditions, overlapping, one increment per compile each holds for |
| `loop_xform_eligible` | none of the four held |
| `loop_xform_not_armed` | …and the rewriter was not armed. Equals `loop_xform_compiles` on a default run |
| `loop_xform_no_candidate_loop` | armed and eligible, but no loop passed the band and the structural test |
| `loop_xform_planner_refused` | armed and eligible, a loop was chosen, the rewriter refused it |
| `loop_xform_applied` | rewritten bytecode was compiled |

The bottom four need `CRATONVM_JIT=bytecode-loop-xform` to be anything but zero;
see `docs/jit/loop-rewriter-wiring.md`.

## The trap in measuring this

Arming the rewriter **also disables the native byte-copy unroller** — the two
are exact complements. So any A/B that arms the rewriter is changing two things
at once, and "the code got longer/shorter" proves nothing about whether a
bytecode transform happened. That is why this is a counter and not an artifact
diff.

## What to refuse

Do not relax a gate without the translation it was protecting. Each of the four
names a real path where an emitter pc reaches the VM as a resume bci; the gate
is the only thing standing between that and a resume at the wrong bytecode.
Relaxing one means implementing the translation for it, and proving the
translation with the same shape of test the deopt-stub bci translation got —
one accessor, four baking sites, and a test that a transformed method's recorded
bcis are all in interpreter space.

And do not relax one because the tally says it is cheap. The tally says which
gates *cost* something, not which are *safe* to remove; those are different
questions and only the second one is about soundness.
