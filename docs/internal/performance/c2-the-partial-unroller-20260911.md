# The partial unroller, and the two bugs it was the first code to reach

**2026-09-11.** Two pages before this one
([`c2-unrolling-is-a-deopt-metadata-problem-20260911.md`](c2-unrolling-is-a-deopt-metadata-problem-20260911.md),
[`c2-per-copy-deopt-frames-20260911.md`](c2-per-copy-deopt-frames-20260911.md))
ended with the same owed item. The first measured that **every counted loop in
CratonBench and CratonBenchC2 has a runtime bound** — 13 loops, 6 of them
counted, all 6 runtime-bound — which is the population full unrolling can never
serve, and named a partial unroller as the thing that would. The second landed
the deopt metadata that unroller needs and said in its own headline that it
**wins nothing yet**: *"The prize is the partial unroller it unblocks."*

This is that unroller. It is behind `CRATONVM_JIT_IR_PARTIAL_UNROLL`, **default
OFF**, with `CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR` (default 4, clamped 2..=8).

The two things worth reading this page for are not the transform, which is
short and was designed in §3 of the first page. They are the **two wrong-code
defects it was the first code in this tree to reach**, neither of which is in
the unroller, both of which were live before it and reachable by anything that
ever clones a control node.

## 1. The shape, and the one design decision in it

Read off the single-pass tier's own disassembly rather than assumed: **the loop
test is kept in every copy**, and only the back edge and its safepoint poll are
amortised.

```text
header:
  cmp i, n ; jge exit      <- every copy keeps this
  BODY(i)  ; i += s
  cmp i, n ; jge exit
  BODY(i)  ; i += s        <- `factor` copies
  ...
  poll ; jmp header        <- paid once per `factor`
```

Two properties fall out of keeping the test, and both are why this form was
chosen over the textbook main-loop-plus-remainder one:

* **No trip-count arithmetic**, so no `i + (U-1)*s` to overflow at the top of
  the `int` range and pass a signed test — the hazard the range-BCE closeout
  already has on record.
* **No speculation.** Copy `k` runs only if copy `k`'s own test passed, exactly
  as iteration `k` did before. Cloning a body containing a trapping `Op::Load`
  is therefore no different from leaving it where it was.

### The early exits do not leave the loop

This is the one place the implementation departs from §3's sketch, and it is
the decision the rest of the transform's smallness rests on.

The textbook rendering gives each copy's failing test its own edge **out** of
the loop, which needs a `factor`-way exit merge and, at it, an exit phi per
carried value — and then every post-loop use of a carried phi, in node inputs
**and in safepoint slots**, has to be rewritten to the exit phi while every
in-loop use is left alone. That rewrite is a partition of a use list by "is this
inside the loop", and getting it wrong is not a crash: it is a frame slot naming
the wrong iteration, which is the defect class `internal/fixed-bugs/` records as
an H2 `GROUP BY` returning 3 rows of 5.

So copy `k`'s failing test **branches back to the header**, carrying that copy's
values on a new header predecessor. The header's own test — copy 0's, which is
the original `If`, untouched — then fails on those same values and leaves
through the original exit.

The cost is one redundant header test, once, on the way out of the loop. The
benefit is that **nothing outside the loop changes at all**: `exit_ctrl` keeps
its users, the carried phis keep their identities and therefore their post-loop
uses, and no safepoint slot anywhere in the method needs a rewrite. The
transform is closed under the loop it is given.

A four-way unroll therefore turns a two-predecessor header into a five-
predecessor one (pre-header, one real back edge, three early re-entries), and
each carried phi from three inputs into six. `ir_lower`'s phi machinery is
arity-generic and `ir_verify::check_phis` already enforces the positional
pairing, so neither needed a change.

## 2. The census had to stop calling `runtime_bound` a disposition

`UnrollCensus` closed as a sum over buckets with `runtime_bound` as a term. It
cannot stay one: a runtime-bound loop now goes on to the same control-shape,
side-effect, clonability and frame checks a constant-trip one does, and lands in
whichever of those, or in the new `partially_unrolled`, actually disposed of it.

`runtime_bound` is now a SHAPE count outside the identity, with
`runtime_bound_refused` — declined for being runtime-bound and nothing else — as
its terminal counterpart inside it. **With the flag off the two are equal and
every published number is what it was before.** The gap between them, with the
flag on, is how many loops the partial unroller took responsibility for.

## 3. The first defect: an `If`'s successors were ordered by node id

`ir_lower` names `successors[0]` the TRUE block and `successors[1]` the false
one. `ir_schedule` built that list by scanning the graph for `Proj` users of the
terminator **in node-id order**, under a comment that said "Find Proj(0) and
Proj(1) successors" and code that did not look at the index.

The two agree for every graph `IrBuilder` produces: its `if_icmp*` arms add
`Proj(0)` and then `Proj(1)`, so the lower id is always the true edge. That is a
property of one producer, not of the IR, and **the first transform to clone an
`If` broke it.** The unroller adds each copy's continue-edge projection first —
it is the one the next copy hangs off — and in a javac `if_icmpge` loop the
continue edge is `Proj(1)`.

Every intermediate test then branched to the edge meant for its opposite. The
early exits were never taken, the loop ran a whole group regardless, and it
stopped only at the header:

```text
factor 4:  sum(1) = 6      # ran 4 iterations, not 1
factor 2:  sum(1) = 1      # ran 2
```

i.e. `sum(n + factor - 1)` for every `n`. **A trip count divisible by the factor
hides it completely**, which is exactly what the first probe used — `trip = 24`,
`factor = 4` — and why the first runs looked correct.

Fixed in `ir_schedule` by sorting an `If`'s successors on the projection's own
index, which makes the invariant a property of that code rather than of
node-allocation order, and is a no-op on every graph the builder makes. The
unroller also now adds `Proj(0)` before `Proj(1)`, so the two orders agree
anyway.

Pinned by `an_ifs_successors_are_ordered_by_projection_not_by_node_id`, whose
fixture is built **backwards on purpose**: a conventionally-ordered one cannot
tell a fixed scheduler from a broken one.

## 4. The second defect: an OSR entry resolved a bci two blocks claimed

`emit_osr_entry_stubs` plans one entry per BLOCK, keyed by that block's control
node's bci. `CompiledMethod::ir_osr_entry_addr` resolves one per BCI with
`.find()`. So a bci claimed by two blocks silently resolves to whichever came
first in block order — a resume point that is not a function.

Nothing could claim a bci twice until a transform cloned a control node. A
cloned `If` keeps the original's `bytecode_pc` — it has to, because the pc is
also the site key `ir_lower` looks compact field offsets, direct-call entries
and MIC/PIC pairs up by — so a factor-4 unroll offers **four** blocks at the
header bci and three of them are mid-group.

Entering mid-group re-runs the copies below it, which the interpreter has
already run. Measured on `C2PartialUnrollProbe` with the §3 defect still
present: the wrong answer and a 3x slowdown arrived together, on roughly 40% of
runs, and both vanished under `CRATONVM_JIT_IR_OSR_ENTRY=0` — which is what
identified it.

The fix is not "keep the first plan". The tie-break is **"keep the loop
header"**: an interpreter arriving at a loop's bci is at that loop's header, and
a header's control node is a `Merge`/`Region` while every copy's block starts
with a `Proj`. Where that does not single one block out, the bci is refused
outright — no OSR entry is always correct, and the interpreter simply keeps
running the frame it is in.

Pinned by `a_partially_unrolled_loop_has_one_osr_door_and_it_is_the_header`,
which **enters** rather than counting: a count alone is what a wrong version
also has, because keeping the first plan yields exactly one entry too and it is
the mid-group one. The test enters at the header from a state the method could
not have reached on its own (`s = 1000`, `i = 3`) and asks for the answer the
interpreter would have finished with.

## 5. What it measures: correct, smaller per iteration, and **not faster**

`bench/C2PartialUnrollProbe.java` is the shape and nothing else — a runtime-bound
counted loop with a pure integer body, in a small static method invoked past the
optimizing tier's 20,000-invocation threshold. Windows dev box, not the Azure
bench host, so read the ratio and not the absolutes.

**Correctness first.** Checksums match Temurin 25 on trip counts 23, 24, 25 and
26 — deliberately including ones **not divisible by the factor**, which is the
case §3's defect hid behind.

**Timing**: 9 alternating pairs, order flipped on alternate pairs, 400,000 x 1,001
iterations, medians, no sample discarded.

| | median | min |
|---|---:|---:|
| partial unroll OFF | 356 ms | 329 ms |
| partial unroll ON (factor 4) | 362 ms | 316 ms |

**Ratio 0.98 against a run-to-run spread of ±8%: no measurable difference.**

That is not the transform failing to do what it says. It does exactly what §1
predicts, and the disassembly says so:

| | rolled | unrolled, factor 4 |
|---|---|---|
| C2 body | 702 B | 1,067 B |
| instructions per ITERATION | 20 | 16.25 |
| carried `a`, `i` live in | `rbx`, `r15` | the frame |
| memory operands in the loop | **0** | **69 of 180** |

The back edges and their safepoint polls are gone, and the per-iteration
instruction count falls 1.23x. It buys nothing because **the rolled loop had no
memory operand in its loop at all**, and the unrolled one spills every carried
value.

### Why it spills, which is the finding worth keeping

`ir_schedule::sink_pure_nodes` moves a pure node **only when the loop depth
strictly decreases** — its own doc comment says so, and says the classic
schedule-late "prefers the latest block at equal depth, to shorten live ranges;
that is a different trade with a different risk".

Every copy of an unrolled body sits at the **same loop depth as the header**. So
no copy moves, all four are computed at the top of the group above the first
test, and eight intermediates are live simultaneously against a five-register GP
file (`regalloc::xmm_roles::IR_GP_LINEAR_SCAN`). `[ir] linear scan: 5 values
resident` on both arms — the file is full either way, and the unrolled arm has
three times as many values wanting it.

Two consequences beyond the spills, both visible in the disassembly:

* copy 1's comparison is materialised into a boolean, **stored, reloaded and
  tested** (`setge`/`movzx`/`mov [rbp-0xf8]` … `mov rax,[rbp-0xf8]`/`test`)
  rather than fused into its branch, because the scheduler left the `Cmp` in the
  header while its `If` is three blocks later and `ir_fused_branch` needs them
  together;
* the group computes all four iterations' arithmetic before the **first** test,
  so the final group does up to `factor - 1` iterations of work it discards.
  Harmless (the body is pure) but not free.

### The obvious next step was tried, and is not landed

Schedule-late at equal depth, plus the rule it needs — a phi's value input is
used **on its edge**, i.e. in the matching predecessor block, not in the phi's
own block, and attributing it to the phi's block makes a loop header a use site
for every carried value and pins all of them above the body whatever the depth
rule decides.

Both were implemented and both are **reverted**, because together they make
`ir_lower::lower_inner` refuse the method — fail-closed, so the single-pass body
is kept and nothing is miscompiled, but the feature does not work. The refusal
is unlabelled; finding it is the next piece of work, and it is worth doing:
nothing else in the measurement above is between this transform and its win.

A warning for whoever takes it, paid for once already: the first version of the
flag cached in a `OnceLock`, the matrix test ran the OFF arm first, and **both
arms reported byte-identical code**. `ir_per_copy_frames_enabled` documents this
exact trap. Read a scheduling flag live.

## 6. What is still open

Ordered by what §5 says stands between this and a win, which is **not** the same
as ordered by size.

* ~~**Schedule-late at equal depth** (§5). Until the copies stop being computed
  above the first test, this transform trades back edges for spills and the
  trade is even. Everything else on this list widens reach, and widening the
  reach of a transform that pays nothing is worth nothing.~~
  **BUILT 2026-09-12**, behind `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` (default OFF),
  and it moved the schedule exactly as predicted — `scan_spills` 7→5,
  `scan_reloads` 7→5, `peak_live` 15→13, frame references in the emitted body
  −24%. **It bought no time, and the premise of the sentence above is what was
  wrong**: this transform was never paying nothing. §5's 0.98 was measured
  against a ±8% spread; with a control arm and nine rounds the floor is 0.7–1.0%
  and the same 0.98 is *above* it. The unroller was already worth ~2%, the
  spills it was blamed for are off the probe's dependency chain, and removing
  them changes nothing anyone can time. Two more things had to be fixed to make
  the copies move at all — a phi's value input is used on its EDGE, and the
  safepoint anchor was keyed by bci so every copy's frame claimed every copy's
  blocks. Write-up, including the throughput-bound arm that is still unreported:
  [`c2-schedule-late-at-equal-depth-20260912.md`](c2-schedule-late-at-equal-depth-20260912.md).
* **The clone set is still pure-plus-`Op::Load`.** The shared side-effect scan
  refuses a loop containing an `ArrayLoad`, `ArrayStore`, `Call`, `New` or
  `Guard` that is loop-variant or pinned to the loop. For the full unroller that
  conservatism is load-bearing, because a fully unrolled body runs
  unconditionally. **For the partial unroller it is not**: every copy is behind
  its own copy of the loop test, so cloning an `ArrayLoad` introduces no
  speculation and no reordering relative to what the loop already did. Widening
  the scan for the partial path only is the single largest reach increase
  available here — an array-indexing loop is the shape this transform most wants
  and currently cannot take.
* **An invariant `Op::Load` pinned to the header still refuses the loop**
  (`escapes_or_pinned`), because it is not in the clone set and the copies would
  leave it naming the header. LICM would hoist it, but LICM runs *after* the
  unroller.
* **The default.** This rests on `CRATONVM_JIT_IR_PER_COPY_FRAMES`, which is
  itself default-OFF pending a soak. Both should flip together, on checksum
  parity across the collector and workload matrix, not on the argument above.
