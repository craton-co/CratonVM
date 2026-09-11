# A deopt frame per COPY: the lowerer change the unroller's own comment asked for

**2026-09-11.** The page before this one
([`c2-unrolling-is-a-deopt-metadata-problem-20260911.md`](c2-unrolling-is-a-deopt-metadata-problem-20260911.md))
ended on a single sentence of owed work:

> **A deopt point must be addressable per COPY, not per bci.** […] The shape of
> the fix is a per-copy identity on cloned nodes, a cloned-and-substituted
> snapshot per copy appended to `graph.safepoints`, and an anchor taken from the
> copy's own nodes rather than from `bci_native`.

That is what this is. It is behind `CRATONVM_JIT_IR_PER_COPY_FRAMES`, **default
OFF**, and the honest headline is that **it wins nothing measurable yet** — the
census on the page before says every counted loop in both benchmark suites has a
runtime bound, and full unrolling cannot serve one. This lands the mechanism and
the tests that hold it, on the shape that needs it. The prize is the partial
unroller it unblocks.

## 1. What was actually broken

A cloned node keeps the original's `bytecode_pc`. It has to: the pc is also the
SITE KEY that `ir_lower` looks the compact field offset, the direct-call entry
and the MIC/PIC pair up by, and an earlier draft of the IR splice that gave a
region a different pc measured a 47 ns → 11 000 ns per-element regression and a
wrong-code hazard besides (`ir.rs`, "Two bcis, and why a node keeps the COMBINED
one").

So after an unroll there are `trip` nodes at each body bci, and two mechanisms
collapse them onto one frame:

1. **`resolve_frame_state_for_bci`** — `safepoints.iter().position(|s| s.bci ==
   bci)`. First match wins. Every guard's boxed deopt point goes through it, so
   copies `1..trip` would each carry copy 0's frame.
2. **`bci_native`** — one native offset per bci, kept at the EARLIEST. Copy 0
   gets the table's point; the other copies get none, and `points.dedup_by_key`
   removes any that tied.

(1) is the wrong-code half. A frame is not a crash: the interpreter resumes
holding another iteration's locals and returns an answer.
`internal/fixed-bugs/` records what that looks like from the outside — an H2
`GROUP BY` that returned 3 rows of 5, from a frame slot nothing had written.

(2) is the fail-closed half: a missing point is a whole-method re-run.

## 2. The mechanism, in the three named places

**`Node::frame_snapshot: Option<u32>`** — the per-copy identity, on the node,
because the copy is a property of the node rather than of anything threaded
alongside it. `None` on everything the builder makes, which is every compile that
does not unroll, and the by-bci scan is then the only path. Installed through
`Graph::set_node_frame_snapshot`, which **refuses** a snapshot whose `bci` is not
the node's own `bytecode_pc` — a node bound to a frame that resumes elsewhere is
not a copy identity, it is a mis-substitution, and it is the variety that does
not announce itself.

**`ir_optimize::install_copy_frames`** — per iteration `t`, each body snapshot
substituted through that iteration's `subst` map (body node → this iteration's
clone, carried phi → the value entering this iteration) and each of that
iteration's clones stamped with it. Iteration 0 rewrites its snapshot **in
place** rather than appending: `bci_native` anchors a bci at copy 0's offset, so
if copy 0 got a fresh snapshot and the original were left naming dead nodes, the
original would keep the anchor and the table's point at that offset would
describe an iteration that no longer exists.

**`Lowerer::snapshot_native` + `cur_node_frame`** — the anchor taken from the
copy's own nodes, and the frame preferred at an emission site
(`resolve_frame_state_for_site`). Both are empty/`None` unless something cloned a
region, so `build_deopt_points` is `bci_native` verbatim everywhere else.

Two more places that are not on the original list and turned out to be
load-bearing:

**GVN's identity.** `gvn` merges pure nodes by `(op, ty, inputs)`. Two copies of
one body compute the same value at DIFFERENT program points, and merging them
hands one copy's code the other copy's frame. `frame_snapshot` is now part of
both the hash and the equality check. It is `None` on both sides for every graph
that did not unroll, so it costs nothing there — and it is the reason the test in
§4 can read five distinct constants rather than one shared one.

**`ir_verify::check_frame_states`.** Its duplicate-bci rule existed *because* of
exactly the collapse this change fixes, and said so. It is now: a duplicated bci
is a violation unless every snapshot at it is CLAIMED by some node — at which
point no consumer is resolving it by position and there is nothing to discard.
An unclaimed duplicate is still reported, which is the case the relaxation must
not swallow: it is what a half-finished copy looks like.

## 3. Three kinds of snapshot, and only one is a refusal

The part that took the longest to get right, because the first version refused
the most ordinary loop there is.

`IrBuilder` records a snapshot at **every** bytecode index. A javac loop body
ends `istore_0; iinc; goto`, and `istore` and `goto` produce no node at all. So a
rule of "every snapshot naming the body must be at a bci some clone carries" —
which is what the first draft asserted — declines `for (i = 0; i < 5; i++) a +=
i;` on its `istore`. The census confirmed it: `frame_uncopyable=1`, the loop
refused with the mechanism on.

The line that is actually right runs through whether the bci is **emitted**:

| snapshot at… | treatment | why |
|---|---|---|
| a bci a clone carries | copied per iteration, stamped | the mechanism |
| a bci NO live node carries | left alone | unconsultable: `build_deopt_points` anchors through `bci_native`, which is populated only from nodes, and every deopt this file emits resumes at some node's own `bytecode_pc` |
| a bci some OTHER node carries | **refused** (`frame_uncopyable`) | the code is emitted, it can be resumed at, and the frame names a body value with no iteration to belong to |

The middle row is not a loophole, it is the common case, and copying those
snapshots would be actively worse than leaving them: **safepoint slots are DCE
roots**, so `trip` copies of a frame nothing can read would keep every
iteration's intermediates alive to describe a program point with no code.

`ir_verify` draws the same line, in the same words, on the consuming side.

## 4. The test is executable, and both halves of it were mutated

The graph assertions are on the NUMBERS, because the shape is what a wrong
version also has. For `int f() { int a = 0; for (int i = 0; i < 5; i++) a += i;
return a; }` the accumulator entering iteration `t` is `0, 0, 1, 3, 6` and the
induction variable is `0..4`. After the post-unroll fold every one of those is a
concrete `Op::Const`, so the five snapshots at the `iadd`'s bci read:

```text
sp[9]  bci=11  locals=[0, 0]  stack=[0, 0]
sp[16] bci=11  locals=[0, 1]  stack=[0, 1]
sp[19] bci=11  locals=[1, 2]  stack=[1, 2]
sp[22] bci=11  locals=[3, 3]  stack=[3, 3]
sp[25] bci=11  locals=[6, 4]  stack=[6, 4]
```

A version that shares one frame reads `(0, 0)` five times.

Four tests, and the pairing matters more than the count:

* `each_copy_of_an_unrolled_body_names_its_own_interpreter_frame` — the table
  above, plus that every one of those five snapshots is claimed, plus the
  frame-state verifier.
* `the_same_loop_is_refused_when_per_copy_frames_are_off` — **deliberately
  opposed.** Without it the first test proves nothing about the mechanism: a
  fixture the unroller would have taken anyway satisfies every assertion there
  while the per-copy code does no work at all. This one pins the fixture to the
  `safepoint_named` refusal.
* `an_unrolled_body_carries_a_deopt_point_per_copy` (`ir_lower`) — the half the
  graph tests cannot do. It executes both arms (the unrolled body is code
  nothing else in the suite runs) and asserts **five deopt points at bci 11, at
  five distinct native offsets**, against the rolled arm's one.
* `a_frame_snapshot_must_be_the_nodes_own_program_point` — the fail-closed API.

**Mutation-checked**, because this area's history is a test that could not fail:

| mutation | result |
|---|---|
| `install_copy_frames` substitutes nothing | RED — frames read `(0,0)×5` |
| clones are never stamped | RED — "safepoint[9] … is one of 5 copies and no node names it" |
| `build_deopt_points` prefers `bci_native` | RED — "expected one deopt point per unrolled copy of bci 11, got 1" |

## 5. What the probe found that the source audit did not

Two things, and neither was visible from reading the code.

### A defect in `unroll` that per-copy frames made reachable

`walk(N o) { int a = 0; for (int i = 0; i < 5; i++) { a += o.v; o = o.next; }
return a; }` — the smallest loop with a loop-carried RECEIVER, and one a
snapshot names, so it had never unrolled before. With the mechanism on:

```text
[ir] verifier rejected UT3.walk(LUT3$N;)I: 10 violation(s):
     n24:Load(Int) input[0] = n16 refers to a removed (Dead) node; ...
```

Ten violations — two loads per body, five copies. `unroll` substituted `region`
into each clone's inputs and **not `back_ctrl`**, so every copy of a load pinned
to the back edge kept a control input the teardown then killed. The pass's own
side-effect scan spells the body's control anchors `ctrl == region || ctrl ==
back_ctrl`; the second one was always there to be read.

It **failed closed** — `ir_verify`'s structural lane runs in production, the
compile was refused, and the method took the other tier — which is why the cost
was a probe rather than a wrong answer, and also why no assertion about
behaviour would have found it. The fix is one line of `subst`, plus the same
correction to the `pinned_invariant_load` guard, plus
`a_body_value_pinned_to_the_back_edge_survives_the_unroll`, which asks the graph
and reproduces the production message verbatim when the line is removed.

### A pre-existing SIGSEGV in the SINGLE-PASS tier, on the same shape

Chasing the crash the probe hit turned up something this change has nothing to
do with. Same fixture, list SHORTER than the trip count so `o` goes null inside
the loop:

| arm | `-Dprobe.reps=20000 -Dprobe.len=3` |
|---|---|
| HotSpot (Temurin 25) | `sum=0 traps=20000` |
| `CRATONVM_NO_IR=1` (single-pass only) | **SIGSEGV**, read at `0x0F` |
| `CRATONVM_NO_IR=1 CRATONVM_DISABLE_UNROLL=1` | `sum=0 traps=20000` |
| `CRATONVM_JIT=force-c2` | `sum=0 traps=20000` |

**The single-pass tier's own native unroller drops the null check on a
loop-carried receiver**, turning an NPE into an access violation. Default-ON,
pre-existing, and filed separately — the knob that makes it correct is
`CRATONVM_DISABLE_UNROLL`, which names the right pass.

## 6. The cost nobody asked about, stated before it is discovered

**Per-copy frames keep every iteration's intermediates alive.** Safepoint slots
are DCE roots, so a frame that names copy `k`'s accumulator is a reason for copy
`k`'s accumulator to exist. On the fixture above the whole loop folds to
`Const(10)` — and five `Const` nodes survive at bci 11 anyway, materialised
purely to describe frames.

That is correct (a deopt genuinely needs those values) and it is the direction
the old escape hatch avoided by asserting the frames were unreachable and
dropping them wholesale. The two are not competitors: the hatch is right when
nothing can deopt, this is right when something can. But a future measurement
that finds unrolled code larger than expected should look here first, and the
narrower fix — rooting only snapshots that can be consulted — is a DCE change,
not this one.

## 7. Why it is default-OFF, and what would flip it

Because it is the deopt metadata, and the failure mode is a right-looking wrong
answer rather than a crash. `ir_register_authoritative_enabled` earned its flip
on 39 workloads × 3 collectors with 0 divergent and checksum parity against
Temurin 25 on 14 workloads × 7 arms. This wants the same, and it wants it on the
exception-heavy and deopt-heavy suites specifically, because the guard path is
the one that reads these frames.

Note what the flip does NOT need: the trap-free prediction. The old relaxation
(`CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES` + `…_DROP_UNREACHABLE_HOMES`)
argues the objection cannot be reached and carries a net in `lower_inner` for
when the emission disagrees. This answers the objection instead, so it needs
neither.

## 8. What this is worth today: nothing, and that is the point

`per_copy_frames` is a sub-count of `unrolled` in `UnrollCensus`, published
beside `safepoint_named` so the pair reads together. On the benchmark suites it
will be **zero**, for the reason the previous page measured: every counted loop
there has a runtime bound and bails long before the safepoint refusal.

§7 of that page is still the work. What changed is that the sentence it ended on
is no longer owed.

## 9. The end-to-end run, and the flag it needed

A deopt out of copy `k > 0` IS exercised, and getting there took one more
switch than expected.

The shape that reaches it is the pointer walk from §5 — `for (i = 0; i < 5;
i++) { a += o.v; o = o.next; }` over a THREE-element list, so the receiver goes
null inside copy 3. An invariant load cannot reach it: every copy loads the same
receiver, so if any copy traps, copy 0 traps first and the later frames are
correct but unreached.

On a default run that probe still crashed with per-copy frames on, and the
reason was neither tier's unroller:

```text
[ir] acceptance UT3.walk(LUT3$N;)I: REFUSED (evidence: unrolled)
     -- keeping the single-pass body
```

`ir_evidence::accept` priced the unrolled C2 body as not worth publishing and
handed the method back to the SINGLE-PASS tier — straight into the defect §5
describes. So the C2 body was never running, and the crash was never this
mechanism's. With `CRATONVM_C2_ACCEPT=always`, which installs the body the gate
declined:

| `-Dprobe.len=3 -Dprobe.reps=20000` | per-copy OFF | per-copy ON | HotSpot |
|---|---|---|---|
| pointer walk, null inside copy 3 | `sum=0 traps=20000` | `sum=0 traps=20000` | `sum=0 traps=20000` |
| `UnrollFrames` (pure + invariant + variant) | `1400000` | `1400000` | `1400000` |

Twenty thousand deopts out of a cloned body, each resuming in the copy that
trapped, each throwing the `NullPointerException` HotSpot throws.

That is worth stating plainly for a second reason: **an acceptance gate between
a transform and its execution means "the checksum matched" can be a statement
about code that never ran.** The first pass of these numbers was collected
without `CRATONVM_C2_ACCEPT=always` and proved less than it appeared to.

## 10. Still owed, named rather than left to be found

* **`InlineScopeTable` binds a scope by SNAPSHOT INDEX**, and an appended copy
  snapshot has no entry, so it would resolve to a flat frame where the original
  resolved to a chain. Unreachable today — the table is empty on every compile,
  and no snapshot is pushed inside a splice — but a producer that starts
  building chains must copy the scope binding alongside the snapshot. Two lines
  in `install_copy_frames`, and the reason they are not written yet is that
  there is nothing to test them against.
* **The DCE-rooting cost in §6**, if a measurement ever wants it back.

## 11. Green

* **2364** `cratonvm-jit` unit tests, **145** `ir_vs_singlepass` differential
  tests, and all 16 test targets — run TWICE, once with
  `CRATONVM_JIT_IR_PER_COPY_FRAMES=1` in the environment, because a default-OFF
  mechanism whose ON arm is never compiled against the suite is a mechanism
  nothing has tested.
* Regression suite **92/92** against HotSpot, both arms.
* `CratonBench` `5000000003999999995 701408733 9592 173943680 1549999915000000
  5000050000 68332206` and `CratonBenchC2` `2893201123071733440
  -1727289071355132288 97968176938830464`, both arms, unchanged from the
  pre-change baseline.
* The §9 table, which is the only one of these that runs the new code.
* Three mutations red (§4) and a fourth for the §5 defect: removing
  `subst.insert(back_ctrl, entry_pred)` reproduces the production verifier
  message verbatim.
