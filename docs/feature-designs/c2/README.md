# C2 — the review lanes, and where the optimizing tier actually is

`docs/feature-designs/c2/deep-research-vm-c2.md` was worked through by two waves of
parallel agent lanes. Its decomposed, file-level items landed. Five lanes did
not, and this directory was those five.

All five now have a first increment landed, and their briefs were deleted by the
commits that landed them — restored under [`archive/`](archive/README.md), with
what each one did and did not finish. What replaced them is a set of lanes sized
from a measurement of the tier rather than from a report: see below.

Read the report's remediation banner first. It records what landed, which of
the report's premises turned out to be false, and — the part that matters
here — which lanes have a **first increment** rather than a finished lane.

## Where the optimizing tier actually is — measured 2026-08-03

Read this before reading the lane table below, because the lane table records
what the *review* asked for and this records what the *tier* does.

The five review lanes are closed in the sense that each had a first increment
land. That is not the same as C2 working, and the difference is measurable:

| | |
|---|---:|
| compile requests reaching the admission chain | 1,954 |
| …a C1 request (`optimize=false` — normal tiering, not a gap) | 719 |
| …refused by `ir_compatible` before the builder ran | 248 |
| **admitted to the optimizing pipeline** | **982** |
| **bodies the optimizing backend produced** | **592** |
| admitted and never lowered | **390 (41%)** |

Ten workloads, default configuration, one run each. Full derivation and the
per-opcode breakdown in
[`ir-coverage-survey-20260803.md`](ir-coverage-survey-20260803.md).

So the tier runs, on real code, by default — and declines to lower two of every
five methods it admits. The binding constraint is **opcode coverage in
`IrBuilder::build` and four whole-method conjuncts in `ir_compatible`**, not
gating and not tiering. Three findings shape the `cov-*` lanes:

> **The opcode half of that sentence is no longer true.** `cov-01` and `cov-02`
> both closed 2026-08-03. On `ConditionalOnPropertyTests` the opcode gap went
> from 196 events to **10** and bodies from 410 to **538 (+31%)**; what is left
> was `cov-04` (82), `cov-03` (43) and the `ir_compatible` conjuncts; both of
> those have since closed too, leaving only the conjuncts. Read the
> combined re-measurement in
> [`ir-coverage-survey-20260803.md`](ir-coverage-survey-20260803.md) before
> sizing anything from the table below.

* ~~**`getstatic` + `ldc`/`ldc_w` is 69% of every opcode gap** (189 of 273).~~
  **Fixed 2026-08-03** (`cov-01`). All three are at zero; `+116` bodies
  (`+20%`) across the three Spring workloads. See the re-measurement in
  [`ir-coverage-survey-20260803.md`](ir-coverage-survey-20260803.md) and
  its closeout.
* ~~**A `float[]` element can be lowered and an `int[]` element cannot.**~~
  ~~`IrBuilder::build` has arms for `faload`/`daload`/`fastore`/`dastore` and for
  no integral or reference array access at all. Those four are what an FP
  kernel needs; the arms that exist are the arms the fixtures demanded.~~
  **Fixed 2026-08-03** (`cov-02`). Every integral and reference array access has
  an arm; `aastore` is the one deliberate exception and says so in the builder.
* **`checkcast`/`instanceof` refuses 306 methods** — more than all 273
  opcode-gap events combined — and they never reach the builder, so they are
  invisible in the opcode histogram.

A second run with `CRATONVM_JIT_FORCE_C2=1` — every request routed to the
optimizing tier — settles the question the `cov-*` lanes rest on, in two parts.
**Correctness: clean.** 886 IR bodies, 61/61 Spring Boot tests, 7/7 CratonBench
checksums against HotSpot, zero panics or new warnings. **Coverage: forcing C2
buys none.** 886 bodies against 595, over *the same 495 distinct methods* — the
two method sets are identical. Forcing C2 changes when the tier is used, never
which methods it can serve, so the `cov-*` lanes are the only lever there is.
Whether an IR body is *faster* than the C1 body it replaces remains unmeasured;
nothing showed the 1.85x regression this project has on record, which is enough
to say the programme is not self-defeating and not enough to say it pays.

And one finding about the measurement itself: **CratonBench issues eight
compile requests to the optimizing tier across all seven phases and gets three
bodies.** The perf gate measures the single-pass backend. That is `meas-02`,
and it is why the array-arm asymmetry survived — the suite that would have
shown it does not reach the tier.

`meas-02` **closed 2026-08-03** — every gate run now records its own per-phase
reach, so that fact travels with the numbers instead of having to be
rediscovered, and `compare.py` names the phases whose delta is not evidence
about the tier. Details, and the two defects the lane turned up on the way,
in `meas-02-bench-suite-c2-reach-RETIRED-20260803.md`.

Eight and three, not the seven and two the original survey recorded, and the
record is what caught both: `stringregex` issues a request it did not before,
and **`cov-02` gave `sieve` a body** by lowering the `0x54 bastore` it used to
die on. A `cov-*` lane moving the bench suite's own reach is exactly the
movement the per-phase record exists to make visible.

## The first thing the coverage lanes cost

**`cov-02` made `CratonBench`'s `sieve` phase 6.4x slower** — 2,462 ms →
15,823 ms, interleaved A/B of the two builds either side of it, checksums
identical. One method: `CratonBench.sieve([ZI)I` was admitted to the optimizing
pipeline on both builds, but only the newer one produces a body for it, and
that body is 6.4x slower than the single-pass one it replaced. CratonVM was
*faster than HotSpot* on this phase before the change.

That is not an argument against `cov-02` — lowering integral array access is
right, and its own closeout measured what it set out to. It is the other half
of the trade, which the survey below had already written down: *"it does not
say lowering these opcodes makes anything faster."* Every lane in the table
below widens the set of methods this can happen to, and nothing today compares
an IR body against the C1 body it replaces before keeping it.

~~`perf-01`~~ **FIXED 2026-08-04**, same day it was found — the optimizing
tier's admission chain now asks the single-pass backend's own detectors whether
they would vectorise a loop in this method, and declines it if so.
Closeout
· [brief](perf-01-sieve-ir-body-6x-slower-than-c1.md).

~~`perf-02`~~ **FIXED 2026-08-05**, same day it was found — and a different
animal from `perf-01`, despite also surfacing as "the IR tier took a bench
method and it got slower". Nothing was wrong with the IR *body*: both backends
erase the dead shadow-stack thread fetch from the prologue, and the single-pass
backend had been jumping over the erased span since June while the IR backend
only NOP-filled it. So **every IR method that published nothing retired 46
one-byte NOPs on entry, on every invocation** — `CratonBench fib`, a two-line
static method entered 2.27e9 times, paid it 2.27e9 times and ran 1.96x slower.
One shared `ExecutableBuffer::erase_range_with_jump_over` now serves both
backends. 1.74x recovered, checksums unchanged on all seven phases. The 1.32x
residual against the pre-regression body is *mostly not a defect*: the frame
record and the per-call safepoint-id store are precise-root and deopt metadata
the IR tier emits and the single-pass backend does not. Only the epilogue
savetop-restore is genuinely dead (~1.32x -> 1.27x); the rest is `perf-01`'s
still-open policy question about replacing a C1 body without evidence.
closeout (`perf-02-ir-thread-fetch-nop-sled-FIXED-20260805.md`)
· [brief](perf-02-ir-methods-run-46-nops-on-entry.md).

Worth keeping the pair in mind together: `perf-01` was a worse body and the fix
was to *decline the method*; `perf-02` was a fine body carrying dead weight and
the fix was to *delete the weight*. A widening lane that makes a benchmark
slower is not automatically evidence against the lane.

**The general form was taken on the same day.** What the single-pass backend
can do and the IR tier cannot is now enumerated in
`jit/src/x64/single_pass_only.rs` — seven classes, each consumed by a
single-pass emitter at a loop header, each without a counterpart in
`ir_optimize`/`ir_lower` — and the admission chain consults the enumeration
rather than a one-off predicate. **The IR tier has no vectoriser at all**, so
`cov-02` hitting one of these was not bad luck: four more of the same shape
were waiting. Widening the veto from three classes to seven still costs
exactly one IR body across both benchmark suites.

Two things that audit turned up, for whoever extends the list:

* **Loop unswitching is detected but not performed** — its emitter's own
  contract says the sequence is additive and "removing the emission yields
  identical final state". It was in the first draft on the strength of its
  name. Do not add a class because a detector and an emitter exist; read what
  the emitter emits.
* **`ir_optimize`'s unroll and LICM are default-ON**, not off as the comments
  beside them claimed until 2026-08-04. That is why the single-pass unroller
  and hoists are not on the list.

**What is still open**: the enumeration catches an advantage somebody wrote
down, not one nobody did. The mechanism that would close that is a
backend-parity harness — compile a corpus both ways and flag any method whose
single-pass body has VEX or `REP`-string bytes its IR body lacks, which is
capability-agnostic. It needs the two backends driven independently over one
method, which `try_compile_inner` does not currently allow. Unowned.

## The coverage lanes

Nine parallel-actionable lanes, each sized from the survey, each with disjoint
ownership. Ordered by measured cost, which is **not** the order to do them in —
read each lane's "first increment".

| Lane | Owns | Events | Notes |
|---|---|---:|---|
| ~~`cov-01`~~ | ~~`ir.rs` arms `0x12`/`0x13`/`0xb2`~~ | ~~189~~ | **CLOSED 2026-08-03** — all three at zero, **+116 bodies (+20%)**. Closeout |
| ~~`cov-02`~~ | ~~`ir.rs` arms `0x2e`/`0x32`/`0x33`/`0x34`/`0x54`/`0x5a`/`0xbe`~~ | ~~77~~ | **CLOSED 2026-08-03** — all seven at zero. Closeout · [brief](cov-02-array-element-access.md) |
| ~~`cov-03`~~ | ~~`ir.rs` arms `0xb4`/`0xb5`~~ | ~~43~~ → **70** | **CLOSED 2026-08-03** — on the tree it landed on (`cov-01`+`02`+`04`) builder refusals **72 → 2** and bodies **588 → 660 (+12.3%)**; the two survivors are `cov-06`'s. The asymmetry was the **write barrier** and only that: a reference LOAD needs none, so the change that taught `getfield` about references had nothing to say about the arm twenty lines below it. Both arms now share ONE tag classifier. Wide `J`/`F`/`D` fields landed too. Closeout · [brief](cov-03-field-stores-and-wide-fields.md) |
| ~~`cov-04`~~ | ~~`ir.rs` invoke arms + `<init>` elision~~ | ~~69 → 81~~ | **CLOSED 2026-08-03** — all three invoke sites at zero. Every one was an `<init>`. Closeout |
| [`cov-05`](cov-05-checkcast-and-instanceof.md) | one `ir_compatible` conjunct | 306 | biggest refusal anywhere; `instanceof` first, `checkcast` needs `cov-07`'s answer |
| [`cov-06`](cov-06-array-allocation.md) | two `ir_compatible` conjuncts + `0xbc`/`0xbd`/`0xc5` | 141 | the conjunct exists *because* the arm is missing — one piece of work, not two |
| ~~`cov-07`~~ | ~~one `ir_compatible` conjunct~~ | ~~89~~ | **CLOSED 2026-08-04** — the question answered itself: `athrow` reuses the exact sentinel-drain protocol `checkcast` (cov-05) already uses, not a second answer to where an exception goes. `scan.has_athrow` refusals 46 → **0** on `ConditionalOnPropertyTests`. Closeout |
| ~~`meas-02`~~ **closed 2026-08-03** | `regression-suite/perf/`, `bench/` | — | the gate records its own C2 reach now — `meas-02-bench-suite-c2-reach-RETIRED-20260803.md` |
| ~~`perf-02`~~ **FIXED 2026-08-05** | `IrLowerer::finish_lazy_thread_fetch` | — | the IR backend NOP-filled the erased shadow thread fetch instead of jumping over it, so every IR method that published nothing ran **46 NOPs on entry**; `fib` 1.96x. 1.74x recovered, checksums unchanged. The 1.32x residual is mostly precise-root/deopt metadata the single-pass body never carried, not dead code; only the epilogue savetop-restore is erasable. Closeout · [brief](perf-02-ir-methods-run-46-nops-on-entry.md) |

Three of them (`cov-05`, `cov-06`, `cov-07`) each delete **exactly one**
conjunct from `ir_compatible`, which is one small function. Four of them
(`cov-01`…`cov-04`) each own disjoint arms of **one match statement** in a
365 KB file. That is as disjoint as this subject gets: rebase daily, land
increments rather than lanes, and do not touch a neighbour's conjunct while you
are in the same function.

**`cov-04` closed 2026-08-03** →
`cov-04-the-invoke-arms-RETIRED-20260803.md`.
It was the one lane whose brief refused to size itself, and the grouping it
demanded first contradicted both cases it had offered. **Every invoke bail was
an `<init>`** — none was the "superclass or private `invokespecial` the arm has
no path for" the brief led with, which inc 24 had already handled. They split
29 / 39 between a compiled constructor's `super(...)`/`this(...)` chain call
(refused by a term whose comment claimed such a method "also has a `new`", which
35 of them did not) and a method containing a `new` (refused by a
`call_eligible` term whose stated reason — that the lowerer has no allocation
path — expired when `ir_lower` grew one). Both terms are gone.

Measured against `origin/dev` at `95152daea`, **with `cov-01` and `cov-02`
already in it**: invoke refusals **106 → 0**, bodies **778 → 849 (+9%)**, and
`cov-03`'s `putfield` row grew 45 → **78**, which was then **78 of the 85**
builder refusals that remained — and `cov-03` closed the same day, taking them
with it. Correctness: the 79-class Spring Boot regression
list, both arms interleaved, run once per baseline — **no class changes state in
either direction** that survives repetition. (Sweep 2 threw one mismatch, in the
*flattering* direction: a devtools class that failed on base and passed on fix.
6/6 PASS on re-run, both arms — a known bean-attribute flake. Re-run a mismatch
that favours you before it becomes a claim.)

Three things to carry into the neighbouring lanes:

1. **Every neighbour that lands makes the next lane bigger.** A method blocked
   on `ldc` never reached its `invokespecial`, so `cov-01`'s landing roughly
   doubled the `0xb7` site. This lane removed 69, then 69, then **106** invoke
   refusals across three baselines — same code, same corpus, different
   neighbours. Do not quote a lane's size without naming the tree it was
   measured on.
2. `ir.rs:5264`'s 13 events were **not** "the site was never resolved at compile
   time". Their callees are `StringBuilder.append` and `Class.getName`; they
   were collateral from a whole-method discard triggered by a constructor
   elsewhere in the same method. A bail site names the *first* thing the builder
   could not lower, which is rarely the thing that caused it — so read a
   structural-refusal row as "where the method died", never as "why".
3. Both removed terms carried a comment stating a premise that was false when
   read and had been true when written. Rule 1 applies to a term's *comment*
   just as much as to a report's claim.

**With `cov-01`, `cov-02` and `cov-04` closed, `cov-03` was the whole remaining
builder story** — 78 of 85 refusals — and the opcode gap was down to **13 events
across the entire corpus**.

**`cov-03` then closed too** (2026-08-03), measured on exactly that tree
(`fb33aa5ac` → `84b519382`): builder refusals **72 → 2**, bodies
**588 → 660 (+12.3%)** on `ConditionalOnPropertyTests`. The two survivors are a
`new` whose site is `JitNewSite::Deferred`, which is `cov-06`'s. **The builder's
structural refusals are, on this corpus, done.** Everything left is in
`ir_compatible` — `cov-05`, `cov-06`, `cov-07`.

The lane was sized at 43 and was 70 when it landed, entirely because `cov-04`
admitted the constructors that write reference fields. Its own earlier numbers
(43 → 38 bodies, measured before `cov-01`/`cov-02`) are in its closeout with the
tree each was taken on named. **Do not quote one against another.**

**Re-run the survey after any of `cov-05`/`cov-06`/`cov-07` lands.** Lifting a
whole-method conjunct admits methods that were hiding behind it, and they fail
on whatever opcode gap they meet next — so the `cov-01`/`cov-02` rankings will
move, and the shortfall between "admitted rose by N" and "bodies rose by less
than N" is the result, not a regression.

**`cov-01` and `cov-02` both proved that rule one level down, and neither is a
conjunct lane.** Each landed 2026-08-03, independently, and each moved the
other's ranking:

* `cov-02` took the three Spring workloads from 591 bodies to 652, and pushed
  its methods onto the next gap: `newarray` 1 → 5 (`cov-06`), `getstatic`
  91 → 92 and `ldc` 90 → 92 (`cov-01`), plus two `aastore` and one `dup2` that
  belong to nobody.
* `cov-01` removed 155 opcode-gap events and produced only 91 bodies; the
  difference surfaced in `cov-04`, whose largest refusal doubled from 36 to 71
  without anyone touching it.
* `cov-03` was measured on three trees and the answer grew each time: 43 events
  before `cov-01`/`cov-02`, 43 with them, **70** with `cov-04` as well. On the
  last, refusals went **72 → 2** and bodies **588 → 660**, and — unlike the
  first two — *nothing* was lost to a neighbour, because there is no longer a
  downstream builder gap for those methods to fall into. **The re-ranking rule
  corrects upward as well as downward, and upward is the direction nobody
  checks.**

So: **re-run the survey after ANY lane lands**, not only the three conjunct
ones. And a caveat that follows from those two landing in parallel — **neither
lane's post-measurement includes the other**. `cov-01`'s +116 was measured
against a base without `cov-02`, and `cov-02`'s +61 against a base without
`cov-01`; the combined figure is in
[`ir-coverage-survey-20260803.md`](ir-coverage-survey-20260803.md) and is the
one to size `cov-03`/`cov-04`/`cov-06` from. The table above predates all of it.

`cov-01` also produced a second-order effect worth expecting from every
remaining lane: **a method moving from C1 to C2 leaves the reach of every
single-pass-only capability.** `vm/tests/pgo02_guarded_virtual_inline.rs` was
green because the IR builder refused `getstatic`, which kept its
`getstatic; invokevirtual` fixture on the backend where guarded monomorphic
inlining is planned. When `getstatic` lowered, the fixture was compiled by C2,
its `inline_tally` was empty, and the test failed. The test now pins the tier
it means. Nothing in the *product* regressed — the flag it drives is
default-off, and the IR tier serves that site from a MIC/PIC cascade — but the
next lane should expect the same class of surprise and check for it.

## The residuals the closed lanes left

Named here because a residual inside a "closed" row does not read like work.
Details and provenance in [`archive/README.md`](archive/README.md), which also
restores the nine original briefs.

| From | Residual | Tracked in | Owner |
|---|---|---|---|
| ~~`hir-02`~~ | ~~six 32-bit isel pattern rows; `Rule::Lea`/`AluImm` fire zero times on real code~~ **CLOSED 2026-08-04** — the rows landed and `Rule::Lea` now fires (26 tiles on CratonBenchC2, against zero), and the selector has a production caller: `CRATONVM_JIT=ir-isel-emit`, with a byte-equality oracle that found 0 disagreements. `AluImm` had a different cause than the row list implied — a safepoint snapshot pins the constant, so the tile cannot absorb it, and the cost model then prices the instruction without the operand load | `hir-02-mir-regalloc-handoff-RETIRED-20260804.md` | — |
| `hir-02` | emitting the rules byte equality cannot cover (`AluImm`, `Lea`) — they need a differential-execution oracle, not a byte one, because dropping a frame load is the point. Verify mode already reports the size of the prize | same | nobody |
| ~~`hir-02`~~ | ~~increment 3 (real registers) — the prologue save area is tractable; the change that would give it a consumer is on the design doc's own "what to refuse" list~~ **PREREQUISITE CLOSED 2026-08-04** — the save area landed *with* three consumers (the linear-scan file reaching XMM7, an FP value living across a call on Windows, and the vector-pool separation), which is exactly the bar the earlier refusal set. The **GP** register class is still unbuilt, and the blocker changed: not the prologue, but that a GP class turns "no reference is register-resident at a GC safepoint" from a structural fact into one that must be proved per site | same | — |
| ~~`hir-02`~~ | ~~the first increment's "an allocation over it, verified by `verify_allocation`"~~ **CLOSED 2026-08-04** — `ir_lower::verify_mir_allocation`. 25 methods, **406 values** located and verified on CratonBenchC2, 0 rejected. Three-valued verdict (`Verified`/`NothingToCover`/`Indescribable`), reported separately on `[ir-isel] MIR ALLOC` | same | — |
| ~~`hir-02`~~ | ~~hazard 2: the vector pool overlaps `ir_lower`'s FP scratch pair and its whole linear-scan file~~ **CLOSED 2026-08-04** — pool moved to XMM8–XMM15 on the back of the save area; `xmm_roles::disjointness_violation()` returns `None` | same | — |
| `hir-02` | increment 3's **GP** register class — gated on the safepoint obligation, and on a measurement showing the frame round trip is a material cost. `mir_totals::read_unlocated_tile_roots` is the first number that bears on it | same | nobody |
| ~~`pgo-02`~~ | ~~bimorphic splicing, a deopt-capable guard, `StableType` invalidation, the metrics harvest~~ **CLOSED 2026-08-04** — and the shipped increment turned out to be WRONG CODE: the guard named one class and the body came from another. Bimorphic emits; the metrics harvest was already done; `StableType` is wired. A deopt-capable guard is the one item still open, and is now mechanically refused rather than merely undone | `../../internal/pgo-02-guarded-inlining-RETIRED-20260804.md` | — |
| ~~`osr-01`~~ | ~~the second compile door — `compile_osr_artifact` calls `x64::compile` directly~~ **CLOSED 2026-08-04**, with the brief's two other unfinished asks (the pc-space newtype, the frame-view cross-check). There were **three** doors, not two; neither direct one had ever checked the code-cache cap; and the OSR door's compile-epoch witness was ~1,000 lines late, after its own class loading. Brief retired to `osr-01-entry-metadata-contract-RETIRED-20260804.md` | `docs/feature-designs/jit-osr-entry-metadata.md` | — |
| ~~`osr-02`~~ | ~~the exit-state differential (its forcing lever, `CRATONVM_OSR_EXIT_AFTER=N`, already exists)~~ **CLOSED 2026-08-04** — `probes/OsrExitDifferentialProbe.java` + `regression-suite/perf/osr-exit-differential.sh`: 15 arms byte-identical to HotSpot with the forced-exit arms taking real entries and real exits, and the historical defect **injected and caught** (200 000 requested, **200 006** executed, while the sum — a pure function of `n` — stayed identical). `osr_exit_points` is cross-checked, and turned out not to mean what its name says: the `invokedynamic` trap shares the exit-map emitter, so membership alone cannot tell a completed iteration from a body that traps | `osr-02-exit-and-recompile-RETIRED-20260804.md` (internal) | — |
| ~~`osr-02`~~ | ~~the differential is a Java-level oracle, not a frame comparator~~ **CLOSED 2026-08-04** — `CRATONVM_DBG_OSR_FRAME_TRACE` records back-edge arrivals, OSR entries and the resumed frame in one format, and `regression-suite/perf/osr-frame-comparator.py` maps every record of an OSR run to its index in an un-compiled run's trajectory by EXACT frame equality. Writing it found that strict monotonicity alone does **not** catch a replay — compiled iterations produce no arrival records — which is why the ENTRY frame is recorded and `index(X) - index(E)` is a second assertion. The comparator has its own `--selftest` over six transcript pairs with known verdicts | `osr-02-exit-and-recompile-RETIRED-20260804.md` (internal) §8 | — |
| `osr-02` | the de-speculation reason lookup at the OSR-exit *reject* sink is still by bci and still takes the first of several. `osr_entry_reason_ambiguous_image` measures how often that pick is arbitrary (non-zero on CratonBench); it is tolerated because admission makes that sink unreachable for an admitted entry — an argument about reachability, not about the lookup being right | same | nobody |
| `loop-01` | unswitching, interchange, fusion — but the binding constraint is now the loop band and structural admission (`no_candidate_loop` is 94%+ of eligible compiles), not the gates `loop-02` retired | `archive/loop-01-peeling-and-versioning.md` | nobody |
| `verify-01` | still stands as the harness every lane above wants | `verify-01-differential-harness-RETIRED-20260803.md` | nobody |
| ~~`cov-01`~~ | ~~`J`/`D`/`F` statics are refused~~ **CLOSED 2026-08-04** — the gate moved to the feed, which is the only party that knows the width. Worth **nothing measurable** (the refusal fired 0 times on the surveyed workload) because a `static final` primitive with a constant initializer emits `ldc2_w`, not `getstatic` | `cov-01-constants-and-statics-RETIRED-20260803.md` | — |
| `cov-01` | `putstatic` (`0xb3`) has no IR lowering; 1 measured event, deliberately out of scope (the SATB pre-barrier) | same | nobody |
| `cov-01` | the "fails to rewrite" half of the reference-root test is unreachable until `JIT_PUBLISHES_RELOCATION_CONTRACT` flips | same | nobody |
| `cov-01` | guarded virtual inlining (`pgo-02`, default-off) no longer reaches a `getstatic; invokevirtual` method — that shape is C2's now. **Measured 2026-08-04**: 72% of a virtual-dispatch workload's installed bodies go to the optimizing tier, which plans no inline, so this feature's population is the methods C2 refuses. `regression-suite/perf/guarded-inline-reach.sh` | `docs/feature-designs/profile-guided-inlining.md` §9 | nobody |
| `cov-03` | no barrier-free fast path for a C2 reference store — every one is a helper CALL. The single-pass backend has an opt-in inline route that proves four premises (mapped, genuinely compact, YOUNG receiver, old field null) this tier cannot yet prove | `cov-03-field-stores-and-wide-fields-RETIRED-20260803.md` | nobody |
| `cov-03` | no inline route for a wide field READ — `emit_inline_compact_getfield` refuses `J`/`F`/`D`, so every one is a helper call plus the sentinel cold branch | same | nobody |
| `cov-03` | a wide value's reconstruction at a deopt is **not observable from Java**: the only shape that would read back a rebuilt `long` (catch the div-by-zero in the same method) is refused by BOTH backends at `rbc6-handler-reads-unsafe-local`, before the admission chain | same | nobody |
| `cov-03` | **whether an optimizing body is FASTER than the single-pass one it replaces.** Every `cov-*` lane moves methods onto a tier that keeps integers in frame slots by default (`ir-linear-scan` is default-OFF), and the survey these lanes are sized from is a COUNT. `cov-03`'s own n=17 A/B could not resolve a difference either way. A coverage win is not a performance win, and no lane owns tier code quality | `performance/`, `reference_c2_tier_slower_because_fields_take_the_helper` | nobody |


## The five review lanes, as the review framed them

| Lane | Docs | Why it is untouched |
|---|---|---|
| HIR/LIR/MIR | ~~`hir-01`~~ ~~`hir-02`~~ **both closed 2026-08-03; `hir-02`'s residuals retired 2026-08-04** | Consolidated into `docs/feature-designs/jit-machine-level-and-instruction-selection.md`. Four levels, not three; the report's "HIR" is the bytecode. Increment 0 (shadow selection, emits nothing) measured **15.7–19.0%** coverage with `Rule::Lea`/`AluImm` firing **zero** times. Both row gaps are now closed and the selector has a **production caller** — `CRATONVM_JIT=ir-isel-emit`, default off, fail-closed, with a byte-equality oracle that ran over a real workload and found 0 disagreements. `Rule::Lea` fires 26 tiles where it fired none. Increment 3 (registers) is deliberately unbuilt and says why. See `hir-02-mir-regalloc-handoff-RETIRED-20260804.md` in the internal tree. |
| Profile-guided inlining | ~~`pgo-01`~~, ~~`pgo-02`~~ | Both lanes CLOSED — first increments 2026-08-03, `pgo-02`'s residuals 2026-08-04. Monomorphic AND bimorphic guarded virtual/interface inlining are real (behind `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`, default-off, and needing `CRATONVM_TIER_PGO` for the profile that feeds them). Only a deopt-capable guard remains, blocked on `FrameState::caller`. See `docs/feature-designs/profile-guided-inlining.md`. |
| OSR | ~~`osr-01`~~ **fully closed 2026-08-04**, ~~`osr-02`~~ **fully retired 2026-08-04** | `osr-01` is finished, brief retired to `osr-01-entry-metadata-contract-RETIRED-20260804.md`, closeout in `docs/feature-designs/jit-osr-entry-metadata.md`. Four findings: there are **three** coordinate spaces, not the two the brief names (and one of the three is a local index, so the brief's "assert all three agree on length" cannot be written as stated); there are **three compile doors**, not two — `try_compile`, `compile_osr_artifact` and the interpreter's eager first-call compile — and **neither direct door had ever checked the code-cache cap**; the OSR door's compile-epoch witness was opened ~1,000 lines late, after its own class loading, so a redefinition landing in that window produced a body the install barrier accepted; and **every counter this lane cited as evidence had no caller anywhere in the tree** — they now print under `CRATONVM_DBG=jit-method-stats`. `osr-02` is **done**: the per-pc livelock memo was already built, the lifecycle counters landed 2026-08-03, and the exit-state differential — the increment the lane called its point — landed 2026-08-04 with the historical defect injected and caught. Two things the writing changed: `osr_exit_points` conflates the loop-boundary map with the `invokedynamic` trap (one emitter writes both), and refusing an ambiguous resume bci on `(semantics, reason)` cost `CratonBench.matrixKernel` its OSR permanently — the predicate is `semantics` alone. See `osr-02-exit-and-recompile-RETIRED-20260804.md`. |
| Loop transforms | `loop-01` **both increments landed 2026-08-03**, `loop-02` **closed 2026-08-03** | Peeling is reachable (the bypassable-header arm) and guarded versioning exists — a pre-header check from `scev::PreheaderGuard`, the transform on the guarded path, an untouched copy of the loop on the fallback. It is *executable*, and since `loop-02` it is executable under the DEFAULT JIT configuration: `CRATONVM_JIT='bytecode-loop-xform'` alone, no `deopt-real=0`. Three of the four whole-compile refusals are gone (`DeoptimizationPoint::bci` is now published through the rewrite's provenance map), which took `loop_xform_eligible` on Spring Boot from 0% to 95–98%; `InlineSitesPresent` stays, measured at 1.8–4.8% of compiles and not worth the inline-scope work. Unswitching, interchange and fusion are unbuilt, and the binding constraint on them is now the loop band and structural admission (`no_candidate_loop` is 94%+ of eligible compiles), not the gates. |
| Loop transforms (measurement) | `loop-02` **measured 2026-08-03** | `deopt_real` fires on **100%** of compiles on Spring Boot (206 / 137 / 865 across three autoconfigure test classes); `invokedynamic` 1.5-7.5%, inline sites 0.7-3.9%, precise exception frames 0-0.8%, and `eligible` is **0** in all three. Narrowing `InlineSitesPresent` — that doc's own suggested first target — would therefore move nothing. Read the tally with `CRATONVM_DBG=jit-method-stats`. Turning `deopt_real` off — the only configuration the transform can run in — used to SIGSEGV on real code; **fixed 2026-08-03** (`deopt-real-off-indy-stub-spilled-over-return-address-FIXED-20260803.md`): the `invokedynamic` trap's stub spilled 32 registers over the caller's return address because the frame reserved that region on a different condition than the stub spilled into it. One Spring Boot test still fails under the flag — a wrong answer, not a crash, and a different defect. |
| `x64.rs` / `invoke.rs` seams | ~~`seam-01`~~ ~~`seam-02`~~ **both closed 2026-08-03** | `x64.rs` 40,588 -> 2,539 and the interpreter's two files 26,775 -> 8,158 and 24,817 -> 3,941, across 17 and 11 verified commits — `seam-01-x64-backend-split-RETIRED-20260803.md` and `seam-02-invoke-dispatch-split-RETIRED-20260803.md`. Neither split found a behaviour bug; between them they found **five checks that name a file where they mean a module**, one of which was already red on `dev`. All five failed closed — assume a fail-open one exists. |

Plus `verify-01`, which is not a lane — it is the harness every lane above
needs in order to prove it did not regress anything. Its first increment
shipped 2026-08-03 (`scripts/verify/compare.py` + fixture checks in the H2
and Tomcat runners + real checked-in baselines for H2/Tomcat/Spring Boot) —
see `verify-01-differential-harness-RETIRED-20260803.md`.

## Rules that made the last two waves work

These are not style preferences. Each one is a defect this campaign actually
shipped or narrowly avoided.

1. **Verify the premise before implementing.** Roughly one premise in three in
   the original report did not hold. Three separate lanes discovered their
   subject already existed, had never been compiled, or that the module's own
   doc made a false claim that was hiding the real hazard. Report the delta
   before writing code.
2. **Disjoint file ownership.** Each doc below names the files its lane owns.
   Two lanes editing one file is how a wave loses work.
3. **Fail closed.** Refusing to compile a method beats emitting plausible
   wrong code. This VM has shipped multiple silent-corruption bugs from the
   other choice, and every one of them surfaced far from its cause.
4. **A new behaviour lands off by default**, behind a *declared* flag. An
   undeclared flag is invisible to `-XX:` and to the test override mechanism,
   and the declaration sweep has already had to be re-run once because a flag
   was added 21 minutes after it closed at zero.
5. **A test that cannot fail is worse than no test.** Two examples from the
   wave: an assertion that passed vacuously because the registry it inspected
   was empty, and a premise check that "proved" a transform had fired when it
   was actually observing an unrelated switch. Write down the exact edit that
   would trip each new check.
6. **Do not trust a handover's census.** The JVMTI lane's handover said ~15
   call sites in one file; the real count was 28 across two. Re-derive counts.

`seam-01` closing on 2026-08-03 added two data points to rule 1, both in the
direction the rule warns about. Its doc named two hazards to expect during the
split; **neither occurred**. The `private_interfaces` warning it predicted for
the loop-rewrite refusal enum cannot fire — the enum's only payload type was
widened to the same visibility as the enum some time after the warning was
seen — and no test turned out to depend on file-private access, because a child
module can see its parent's private items. What *did* break the build on the
first commit was not in the doc at all: two tests that read the backend's own
source text with `include_str!`, one of them an emission-site inventory the
object-header shrink navigates by.

## Sequencing

`hir-01` and `verify-01` were the only two with a hard ordering claim: nothing
in the HIR lane should start before `hir-01` settles the contract, and every
other lane is easier to land once `verify-01` exists. The rest are
independent of each other by construction — that is what the ownership tables
are for. (The HIR ordering claim is discharged; `verify-01` still stands.)

**The HIR lane closed 2026-08-03**, both docs, into
`docs/feature-designs/jit-machine-level-and-instruction-selection.md`. Three
things to carry into the other lanes:

1. The three-defect test scored **one of three**, so that migration was never
   justified as a correctness investment — only by a measurement.
2. The measurement then said **stop**: 15.7–19.0% coverage on real compiles,
   with the two rules the migration was *for* firing zero times. A ten-shape
   synthetic corpus had said 38.2% and named the wrong rules. **Do not size a
   lane from a fixture's node mix.**
3. Rule 1 ("verify the premise") turned up *five* stale claims, four of which
   asserted that finished work was unfinished — the failure mode this
   directory's rule 1 was written for, in the direction nobody checks. One was a
   red test on `dev` that predated the lane entirely.
