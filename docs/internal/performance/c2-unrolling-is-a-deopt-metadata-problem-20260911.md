# Unrolling the optimizing tier is not a codegen gap — it is a deopt-metadata gap

**2026-09-11.** `docs/JIT_OPTIMIZATION.md` has said since 2026-09-03 that *"the
optimizing tier does not unroll"*, priced it at about **1.12x** on a counted
loop, and listed it as the largest remaining item in that tier's per-iteration
instruction budget. All three statements are true. The implied conclusion —
that an unroller needs writing — is not.

**The unroller exists, it is correct, and it recognises a javac counted loop.**
What it then does is refuse it, and every route to making it stop refusing runs
through the same place: a cloned loop body has several copies of each bci, and
`build_deopt_points` can anchor only one deopt point per bci.

This page is the measurement that says so, and the reason no transform was
written: **both of the safe gates a partial unroller could use measure ZERO**
on CratonBench, CratonBenchC2 and the probe set.

## 1. The premise, checked first

`ir_optimize::unroll` full-unrolls small constant-trip counted loops and has
been default-ON since the `activate-ir-optimizer` increment 3. Run against a
real javac loop built from bytecode — `for (i = 0; i < 5; i++) a += i;`, which
unlike the module's hand-built fixtures carries **15 safepoint snapshots**:

```text
[SCRATCH] safepoints=15 nodes=19  trap_free=true
[SCRATCH] header 3 back_inputs=[14]
[SCRATCH] analyze -> Some     trip=5 init=0 stride=1
[SCRATCH] unroll fired: false
```

**The loop is recognised exactly** — trip, init and stride all correct — and
then declined. The decline is silent: it is the `body_named_by_safepoint`
refusal, whose escape hatch is

```rust
unroll_over_unreachable_frames() && trap_free
    && crate::ir_lower::ir_drop_unreachable_homes_enabled()
```

and `ir_drop_unreachable_homes_enabled()` is **default OFF**
(`Err(_) => false`). With `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES=1` the same
fixture unrolls and the loop disappears.

**That flag is off for an unrelated reason, and its sibling has already moved.**
`ir_register_authoritative_enabled`'s own doc says it landed off *"because the
prediction it rests on is the same one `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES`
rests on and that had been off since it landed"* — and it was then soaked (39
workloads × 3 collectors, 0 divergent; checksum parity against Temurin 25 on 14
workloads × 7 arms) and flipped ON on 2026-09-09. The flag it names was never
revisited.

So the first finding is small and cheap: **on a default run this pass refuses
loops it fully understands, because of a flag that is off for a reason that has
since been discharged elsewhere.** Flipping it is a separate question with its
own soak, and it is not what the rest of this page is about — because it would
not reach the loops that matter.

## 2. The census

`ir_optimize::UnrollCensus` — one counter per `continue`, under an accounting
identity `publish_unroll_census` debug-asserts, published per compile so it
cannot race the background compiler. Printed as `[c2-supersede] ir unroll:`
under `CRATONVM_DBG=jitc`, outside the supersede gate.

Two sub-counts sit deliberately outside the identity, because they size work
rather than explain a refusal:

* **`runtime_bound`** — a counted loop whose bound is a VALUE. Full unrolling
  can never serve one; partial unrolling is what would, and it is unbuilt.
  Counting these as "not a counted loop" would have hidden the entire
  partial-unroll population inside a non-event.
* **`runtime_bound_trap_free`** and **`runtime_bound_pure_body`** — the two
  gates under which a partial unroller could be written without touching deopt
  metadata at all. §4.

**A warning about the denominator, stated here because the printed line is easy
to misread.** `merges` counts every `Region`/`Merge` with two control inputs,
and most of those are if/else joins, not loops. They land in
`not_single_backedge`. **The count of loops actually found is
`merges − not_single_backedge`.**

| | CratonBenchC2 | CratonBench | `FieldLoop` |
|---|---:|---:|---:|
| two-input merges | 35 | 8 | 1 |
| …of which not loops (`not_single_backedge`) | 22 | 2 | 0 |
| **loops found** | **13** | **6** | **1** |
| `not_counted` (shape this pass does not model) | 7 | 2 | 0 |
| **`runtime_bound`** | **6** | **4** | **1** |
| `safepoint_named` | 0 | 0 | 0 |
| everything else | 0 | 0 | 0 |
| **unrolled** | **0** | **0** | **0** |

**`safepoint_named` is zero**, which corrects what §1 might suggest: on these
workloads no loop even reaches that refusal, because every counted loop found
has a runtime bound and bails earlier. §1's refusal is real and is what stops
the pass on a constant-trip loop; it is not what stops it on a benchmark.

**Every counted loop in both benchmark suites has a runtime bound.** That is not
a surprise once stated — it is what a Java `for` loop over a length or a
parameter is — and it means the existing pass is serving a population that
barely exists in compiled code.

## 3. So a PARTIAL unroller is the thing. It was designed, and not built

The shape to build is the single-pass tier's own, read off its disassembly
rather than assumed: **it keeps the loop test in every copy** and amortises only
the back edge and the safepoint poll.

```asm
header:
  cmp i, n ; jge exit      <- every copy keeps this
  BODY(i)  ; i += s
  cmp i, n ; jge exit
  BODY(i)  ; i += s        <- U copies
  ...
  poll ; jmp header        <- paid once per U
```

That shape needs **no trip-count arithmetic**, which is where the soundness risk
in the textbook main-loop-plus-remainder form lives: the range-BCE closeout
already records that `i + (U−1)·s` overflows at the top of the `int` range and
that the wrapped value passes a signed test. Avoiding that question entirely is
worth the smaller win, and the win is exactly what the single-pass tier gets,
which is the 1.12x on record.

Per iteration it removes `(U−1)/U × (poll 2 + back edge 1)` — for `U = 4`, 2.25
instructions of `FieldLoop.sum`'s 23.

The clone machinery is already there: `unroll`'s topological order plus
substitution map is directly reusable. What a partial unroll adds is U−1 extra
`If`/`Proj` pairs, a Merge of the U exit edges, and exit phis for the carried
values.

**It was not built, because of §4.**

## 4. Both safe gates measure zero

Cloning a body duplicates its bcis. `build_deopt_points` anchors each point at
`bci_native[bci]`, and `bci_native` keeps the **earliest** native offset for a
bci (`.and_modify(|e| if here < *e { *e = here })`). `find_deopt_point` is an
**exact-offset binary search** returning `None` for anything else. So copy 0
gets the deopt point and copies 1..U−1 get none — and a trap inside copy 2 has
no frame state to reconstruct. The existing pass's own comment says what that
needs: *"a per-iteration snapshot index, which is a lowerer change."*

A partial unroller can dodge that obligation entirely if **no cloned node can
deopt**, because then no copy needs a point. There are two ways to ask that, and
both were measured:

| gate | what it asks | CratonBenchC2 | CratonBench | `FieldLoop` |
|---|---|---:|---:|---:|
| `runtime_bound_trap_free` | is the whole METHOD trap-free? | **0** of 6 | **0** of 4 | **0** of 1 |
| `runtime_bound_pure_body` | are the cloned NODES all pure? | **0** of 6 | **0** of 4 | **0** of 1 |

The second gate is the one worth having built, because the first is obviously
too strong — `graph_cannot_deopt` is false for any method containing a single
call, field access or array access *anywhere*, which is nearly every method. The
per-body question is the real obligation, and it is narrower by a lot.

**It is still zero.** And the reason is the same one that makes these loops worth
unrolling: `FieldLoop.sum`'s body IS a field read, and `Op::Load` is not pure. A
counted loop whose body never touches memory is not a shape real Java produces
in any quantity.

So a partial unroller written behind either gate would fire on **nothing**, and
would join the four zeros `JIT_OPTIMIZATION.md` already records. That is the
reason this page has a census and no transform.

## 5. What unrolling actually needs

One thing, and it is the same thing for both unrollers:

**A deopt point must be addressable per COPY, not per bci.**
`DeoptimizationPoint` already carries `(native_offset, bci, frame_state)` and
nothing stops two points sharing a bci at different offsets — the representation
is fine. Two things collapse them:

1. `bci_native: HashMap<bci, offset>` keeps one offset per bci, so only the
   first copy is anchored;
2. `graph.safepoints` has one `SafepointSnapshot` per bci, naming the ORIGINAL
   nodes, so a later copy has no frame state describing its own values.

The shape of the fix is a per-copy identity on cloned nodes, a cloned-and-
substituted snapshot per copy appended to `graph.safepoints`, and an anchor
taken from the copy's own nodes rather than from `bci_native`. That is a
bounded change to three named places, and it is the prerequisite for every
version of this feature.

It is also the riskiest kind of change this VM has: `internal/fixed-bugs/`
records a deopt-metadata defect that returned 3 rows of 5 from an H2 GROUP BY
query by reading a frame slot nothing had written. Anyone taking it should
expect the test to be an executable differential over `n % U != 0`, not a source
audit.

## 6. What was landed

The census, the typed refusals it needed, and this page. Specifically:

* `UnrollCensus` + `publish_unroll_census`, with the closing identity;
* `NotCounted` — `analyze_counted_loop` returns a *reason* rather than `None`,
  so `RuntimeBound` is separable from "not a counted loop". That distinction is
  the whole finding: the two look identical in a bare zero and call for
  completely different work;
* `partial_unroll_body_is_pure` — the census-only probe that sizes the gate a
  partial unroller would need, so the next person does not have to build the
  transform to discover it is worth nothing.

No behaviour changed. Every number above is a count.

## 7. Reproducing

```bash
cargo build --release -p cratonvm-cli

CRATONVM_JIT=force-c2 CRATONVM_DBG=jitc \
  ./target/release/cratonvm -Xmx4g -cp bench-classes CratonBenchC2 2>&1 \
  | grep 'ir unroll'

# §1 — the refusal, and the flag that lifts it
javac -d /tmp/pc probes/FieldLoop.java
CRATONVM_DBG_UNROLL=1 CRATONVM_JIT=force-c2 \
  ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=300 FieldLoop 2>&1 \
  | grep DBG_UNROLL
```

## 8. Green

2357 `cratonvm-jit` unit tests, 145 `ir_vs_singlepass` differential tests,
regression suite 92/92 against HotSpot, `CratonBench` and `CratonBenchC2`
checksums unchanged — which is what a census-only change should produce, and is
asserted here rather than assumed because "it only adds counters" is exactly the
claim that is worth checking once.
