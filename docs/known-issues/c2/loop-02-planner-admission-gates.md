# LOOP-02 — revisit the four whole-compile refusals

**Status:** not started. **Owns:** `jit/src/x64.rs`'s
`plan_bytecode_loop_xform` region only — coordinate with `seam-01`, which wants
to move that file.

## The finding

`plan_bytecode_loop_xform` refuses the whole compile, before it looks at a
single loop, when any of these hold:

* `deopt_real` is enabled,
* precise exception frames are in use,
* the method contains `invokedynamic`,
* the method has inline sites.

Each refusal is defensible: they name constructs that publish an emitter pc to
the VM as a resume bci through a path the wiring does not translate. But
together they are broad enough that **an ordinary unit-test compile hits one**,
and the transform never runs. That was discovered the hard way — a test that
appeared to prove a wrong-code bug in the rewriter was actually observing an
untransformed artifact, because the planner had refused.

Nobody has measured which refusal fires most often on real code, or what
fraction of hot methods each one costs.

## The first increment

**Measure before narrowing.** Add a refusal tally — which of the four fired,
how often, on a real suite run — and publish it through the metrics module
alongside the bailout table, ungated so a default run shows it. That number
decides whether this lane is worth anything at all; if `invokedynamic` accounts
for 95% of refusals on Spring code, the other three are not where the work is.

Then narrow the cheapest one. `InlineSitesPresent` is the likeliest candidate:
the transform's problem with inline sites is table replication, and the
replication primitive is already generic over the payload.

## The trap in measuring this

Arming the rewriter **also disables the native byte-copy unroller** — the two
are exact complements. So any A/B that arms the rewriter is changing two things
at once, and "the code got longer/shorter" proves nothing about whether a
bytecode transform happened. Measure refusals with a counter, not by diffing
artifacts. See `arming-bytecode-rewriter-also-disables-native-unroller` in the
project memory and `docs/jit/loop-rewriter-wiring.md`.

## What to refuse

Do not relax a gate without the translation it was protecting. Each of the four
names a real path where an emitter pc reaches the VM as a resume bci; the gate
is the only thing standing between that and a resume at the wrong bytecode.
Relaxing one means implementing the translation for it, and proving the
translation with the same shape of test the deopt-stub bci translation got —
one accessor, four baking sites, and a test that a transformed method's
recorded bcis are all in interpreter space.
