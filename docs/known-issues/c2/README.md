# C2 — the review lanes, and where the optimizing tier actually is

`docs/known-issues/c2/deep-research-vm-c2.md` was worked through by two waves of
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

* **`getstatic` + `ldc`/`ldc_w` is 69% of every opcode gap** (189 of 273).
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

And one finding about the measurement itself: **CratonBench issues seven
compile requests to the optimizing tier across all seven phases and gets two
bodies.** The perf gate measures the single-pass backend. That is `meas-02`,
and it is why the array-arm asymmetry survived — the suite that would have
shown it does not reach the tier.

## The coverage lanes

Nine parallel-actionable lanes, each sized from the survey, each with disjoint
ownership. Ordered by measured cost, which is **not** the order to do them in —
read each lane's "first increment".

| Lane | Owns | Events | Notes |
|---|---|---:|---|
| [`cov-01`](cov-01-constants-and-statics.md) | `ir.rs` arms `0x12`/`0x13`/`0xb2` | 189 | largest opcode bucket; the caller already supplies every table it needs |
| ~~`cov-02`~~ | ~~`ir.rs` arms `0x2e`/`0x32`/`0x33`/`0x34`/`0x54`/`0x5a`/`0xbe`~~ | ~~77~~ | **CLOSED 2026-08-03** — all seven at zero. [Closeout](../../internal/cov-02-array-element-access-RETIRED-20260803.md) · [brief](archive/cov-02-array-element-access.md) |
| [`cov-03`](cov-03-field-stores-and-wide-fields.md) | `ir.rs` arms `0xb4`/`0xb5` | 43 | `getfield` learned about references; `putfield` twenty lines below did not |
| ~~`cov-04`~~ **CLOSED 2026-08-03** | `ir.rs` invoke arms + `<init>` elision | 68 | see below |
| [`cov-05`](cov-05-checkcast-and-instanceof.md) | one `ir_compatible` conjunct | 306 | biggest refusal anywhere; `instanceof` first, `checkcast` needs `cov-07`'s answer |
| [`cov-06`](cov-06-array-allocation.md) | two `ir_compatible` conjuncts + `0xbc`/`0xbd`/`0xc5` | 141 | the conjunct exists *because* the arm is missing — one piece of work, not two |
| [`cov-07`](cov-07-athrow.md) | one `ir_compatible` conjunct | 89 | framed as a question; **"keep the refusal" is a legitimate outcome** |
| [`meas-02`](meas-02-the-bench-suite-does-not-reach-c2.md) | `regression-suite/perf/`, `bench/` | — | the gate measures C1; do the cheap half first |

Three of them (`cov-05`, `cov-06`, `cov-07`) each delete **exactly one**
conjunct from `ir_compatible`, which is one small function. Four of them
(`cov-01`…`cov-04`) each own disjoint arms of **one match statement** in a
365 KB file. That is as disjoint as this subject gets: rebase daily, land
increments rather than lanes, and do not touch a neighbour's conjunct while you
are in the same function.

**`cov-04` closed 2026-08-03** →
[`docs/internal/cov-04-the-invoke-arms-RETIRED-20260803.md`](../../internal/cov-04-the-invoke-arms-RETIRED-20260803.md).
It was the one lane whose brief refused to size itself, and the grouping it
demanded first contradicted both cases it had offered. **All 68 invoke bails
were an `<init>`** — none was the "superclass or private `invokespecial` the arm
has no path for" the brief led with, which inc 24 had already handled. They
split 29 / 39 between a compiled constructor's `super(...)`/`this(...)` chain
call (refused by a term whose comment claimed such a method "also has a `new`",
which 35 of them did not) and a method containing a `new` (refused by a
`call_eligible` term whose stated reason — that the lowerer has no allocation
path — expired when `ir_lower` grew one). Both terms are gone. Two things to
carry into the neighbouring lanes:

1. `ir.rs:5264`'s 13 events were **not** "the site was never resolved at compile
   time". Their callees are `StringBuilder.append` and `Class.getName`; they
   were collateral from a whole-method discard triggered by a constructor
   elsewhere in the same method. A bail site names the *first* thing the builder
   could not lower, which is rarely the thing that caused it — so read a
   structural-refusal row as "where the method died", never as "why".
2. Both removed terms carried a comment stating a premise that was false when
   read and had been true when written. Rule 1 applies to a term's *comment*
   just as much as to a report's claim.

Measured after, against `origin/dev` **with `cov-02` already in it**: invoke
refusals 69 → **0**, bodies 655 → **683**, and `cov-03`'s `putfield` row grew
37 → **60**, which makes it the largest builder refusal in the corpus by a wide
margin — larger than every other structural refusal combined. That is the
re-run rule below firing: the methods that were hiding behind the invoke terms
are constructors, and constructors write reference fields. Correctness: the
79-class Spring Boot regression list, both arms interleaved, **zero verdict
mismatches** (65 PASS / 12 pre-existing FAIL, identical sets).

With `cov-02` and `cov-04` both closed, `getstatic` + `ldc` is **96% of the
whole remaining opcode gap** (206 of 214), and the only two structural refusals
left are `cov-03`'s.

**A third re-run trigger, alongside `cov-05`/`cov-06`/`cov-07`:** `cov-04` has
already moved the `cov-01`/`cov-02`/`cov-03` rankings. Re-derive them before
sizing any of those three from the survey's table.

**Re-run the survey after any of `cov-05`/`cov-06`/`cov-07` lands.** Lifting a
whole-method conjunct admits methods that were hiding behind it, and they fail
on whatever opcode gap they meet next — so the `cov-01`/`cov-02` rankings will
move, and the shortfall between "admitted rose by N" and "bodies rose by less
than N" is the result, not a regression.

**`cov-02` already moved them, and the same rule applies to an opcode arm.**
Closing its seven took the three Spring Boot workloads from 591 bodies to 652,
and the methods that used to die at an array opcode now die at the next one:
`newarray` 1 → 5 (`cov-06`), `getstatic` 91 → 92 and `ldc` 90 → 92 (`cov-01`),
plus two `aastore` and one `dup2` that belong to nobody. **Size `cov-01` and
`cov-06` from a fresh survey, not from the table above** — the numbers in it
were measured before 61 more methods started reaching the backend.

## The residuals the closed lanes left

Named here because a residual inside a "closed" row does not read like work.
Details and provenance in [`archive/README.md`](archive/README.md), which also
restores the nine original briefs.

| From | Residual | Tracked in | Owner |
|---|---|---|---|
| `hir-02` | six 32-bit isel pattern rows; `Rule::Lea`/`AluImm` fire zero times on real code | `docs/feature-designs/jit-machine-level-and-instruction-selection.md` | nobody |
| `pgo-02` | bimorphic splicing, a deopt-capable guard, `StableType` invalidation, the metrics harvest | `docs/feature-designs/profile-guided-inlining.md` §8 | nobody |
| `osr-01` | the second compile door — `compile_osr_artifact` calls `x64::compile` directly | `docs/feature-designs/jit-osr-entry-metadata.md` | nobody |
| `osr-02` | the exit-state differential (its forcing lever, `CRATONVM_OSR_EXIT_AFTER=N`, already exists) | `docs/feature-designs/jit-osr-exit-and-recompile.md` | nobody |
| `loop-01` | unswitching, interchange, fusion — but the binding constraint is now the loop band and structural admission (`no_candidate_loop` is 94%+ of eligible compiles), not the gates `loop-02` retired | `archive/loop-01-peeling-and-versioning.md` | nobody |
| `verify-01` | still stands as the harness every lane above wants | `docs/internal/verify-01-differential-harness-RETIRED-20260803.md` | nobody |


## The five review lanes, as the review framed them

| Lane | Docs | Why it is untouched |
|---|---|---|
| HIR/LIR/MIR | ~~`hir-01`~~ ~~`hir-02`~~ **both closed 2026-08-03** | Consolidated into `docs/feature-designs/jit-machine-level-and-instruction-selection.md`. Four levels, not three; the report's "HIR" is the bytecode. Increment 0 (shadow selection, emits nothing) landed and measured **15.7–19.0%** coverage on real compiles with `Rule::Lea`/`AluImm` firing **zero** times — so the next step is six 32-bit pattern rows, not a machine level. |
| Profile-guided inlining | ~~`pgo-01`~~, ~~`pgo-02`~~ | Both lanes' first increments shipped 2026-08-03 — see `docs/feature-designs/profile-guided-inlining.md`. Monomorphic guarded virtual/interface inlining is real (behind `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`, default-off); Bimorphic and a deopt-capable guard remain open. |
| OSR | ~~`osr-01`~~ ~~`osr-02`~~ **both closed 2026-08-03** | The metadata contract is executable and enforced — `docs/feature-designs/jit-osr-entry-metadata.md`. Two findings: there are **three** coordinate spaces, not the two the brief names, and the second compile door (`compile_osr_artifact` calling `x64::compile` directly) is still open and is now the whole remaining item. `osr-02` → `docs/feature-designs/jit-osr-exit-and-recompile.md`: the per-pc livelock memo was **already built** (and is finer-grained than the brief asks — only *artifact-level* refusals may be memoed), and OSR lifecycle counters now make a silent exit distinguishable from never having entered. The exit-state differential is the remaining item; its forcing lever (`CRATONVM_OSR_EXIT_AFTER=N`) already exists. |
| Loop transforms | `loop-01` **both increments landed 2026-08-03**, `loop-02` **closed 2026-08-03** | Peeling is reachable (the bypassable-header arm) and guarded versioning exists — a pre-header check from `scev::PreheaderGuard`, the transform on the guarded path, an untouched copy of the loop on the fallback. It is *executable*, and since `loop-02` it is executable under the DEFAULT JIT configuration: `CRATONVM_JIT='bytecode-loop-xform'` alone, no `deopt-real=0`. Three of the four whole-compile refusals are gone (`DeoptimizationPoint::bci` is now published through the rewrite's provenance map), which took `loop_xform_eligible` on Spring Boot from 0% to 95–98%; `InlineSitesPresent` stays, measured at 1.8–4.8% of compiles and not worth the inline-scope work. Unswitching, interchange and fusion are unbuilt, and the binding constraint on them is now the loop band and structural admission (`no_candidate_loop` is 94%+ of eligible compiles), not the gates. |
| Loop transforms (measurement) | `loop-02` **measured 2026-08-03** | `deopt_real` fires on **100%** of compiles on Spring Boot (206 / 137 / 865 across three autoconfigure test classes); `invokedynamic` 1.5-7.5%, inline sites 0.7-3.9%, precise exception frames 0-0.8%, and `eligible` is **0** in all three. Narrowing `InlineSitesPresent` — that doc's own suggested first target — would therefore move nothing. Read the tally with `CRATONVM_DBG=jit-method-stats`. Turning `deopt_real` off — the only configuration the transform can run in — used to SIGSEGV on real code; **fixed 2026-08-03** (`docs/internal/deopt-real-off-indy-stub-spilled-over-return-address-FIXED-20260803.md`): the `invokedynamic` trap's stub spilled 32 registers over the caller's return address because the frame reserved that region on a different condition than the stub spilled into it. One Spring Boot test still fails under the flag — a wrong answer, not a crash, and a different defect. |
| `x64.rs` / `invoke.rs` seams | ~~`seam-01`~~ ~~`seam-02`~~ **both closed 2026-08-03** | `x64.rs` 40,588 -> 2,539 and the interpreter's two files 26,775 -> 8,158 and 24,817 -> 3,941, across 17 and 11 verified commits — `docs/internal/seam-01-x64-backend-split-RETIRED-20260803.md` and `docs/internal/seam-02-invoke-dispatch-split-RETIRED-20260803.md`. Neither split found a behaviour bug; between them they found **five checks that name a file where they mean a module**, one of which was already red on `dev`. All five failed closed — assume a fail-open one exists. |

Plus `verify-01`, which is not a lane — it is the harness every lane above
needs in order to prove it did not regress anything. Its first increment
shipped 2026-08-03 (`scripts/verify/compare.py` + fixture checks in the H2
and Tomcat runners + real checked-in baselines for H2/Tomcat/Spring Boot) —
see `docs/internal/verify-01-differential-harness-RETIRED-20260803.md`.

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
