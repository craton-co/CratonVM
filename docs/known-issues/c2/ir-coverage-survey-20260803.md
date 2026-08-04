# The optimizing tier's coverage, measured 2026-08-03

Every number the `cov-*` lanes are sized from. One run each, ten workloads,
**default configuration** — no `CRATONVM_JIT_FORCE_C2`, no flags beyond the
Spring Boot runner's usual `CRATONVM_JIT=rootsnap-cache`.

Reproduce with `CRATONVM_DBG=ir-compiles`, which prints one `[ir] admission`
line per compile request with the verdict, and one `[ir] optimizing backend
produced a body` line per success. Both are ungated by `metrics::enabled()`.

`regression-suite/perf/c2-reach.sh` does that counting for any workload in one
run, and refuses rather than reporting a zero when its own consistency checks
say the scrape is reading a log that no longer says what it expects. Prefer it
to hand-grepping: the failure mode of this measurement is a confident zero.

## Headline

**The optimizing tier runs, and 41% of what it admits it cannot lower.**

| | |
|---|---:|
| compile requests reaching the admission chain | 1,954 |
| …refused `optimize=false` (a C1 request — normal tiering, not a gap) | 719 |
| …refused by `ir_compatible` before the builder saw them | 248 |
| **admitted to the optimizing pipeline** | **982** |
| **bodies the optimizing backend actually produced** | **592** |
| admitted but never lowered | **390** |
| distinct methods admitted / with a body | 848 / 497 |

`optimize=false` is **not** a defect and no lane targets it:
`tier_uses_optimized_backend` is `matches!(tier, C2 | FullProfile)`, so a
`false` here is a C1 request doing exactly what tiering says. C2 was asked for
982 times out of 1,954 — the request rate is healthy. The loss is downstream.

## Per workload

| workload | requests | admitted | `optimize=false` | `ir_compatible` | bodies |
|---|---:|---:|---:|---:|---:|
| `ConditionalOnPropertyTests` | 1,372 | 694 | 537 | 140 | 409 |
| `AutoConfigurationSorterTests` | 352 | 182 | 98 | 72 | 123 |
| `ConditionalOnClassTests` | 223 | 103 | 84 | 36 | 58 |
| **CratonBench, all seven phases** | **7** | **3** | **3** | **1** | **2** |
| `CratonBenchC2`, all three phases | 36 | 17 | 16 | 3 | 11 |

The CratonBench row is left at the original survey's numbers. A re-take after
`cov-02` landed measured **8** requests and **3** bodies, not 7 and 2:
`stringregex` issues a request it did not before, and `sieve` now produces a
body it could not. The row is not edited because the three Spring rows above
it were not re-taken, and a table with one re-measured row and three stale
ones is worse than a table with a note. See the next section for the re-take.

That CratonBench row is `meas-02`. The CPU benchmark suite — the thing the perf
gate measures and the README table publishes — issues **eight** compile requests
to the optimizing tier across all seven phases and gets **three** bodies. Every
conclusion this project has drawn about C2 from a CratonBench number was drawn
from a workload that does not reach it.

The `CratonBenchC2` row is the candidate `meas-02` asked for and is
characterised below. It is **not** a gate phase and has no baseline.

## The C2-reach column (`meas-02` increment 1)

Measured 2026-08-03 on the Azure bench host at `50218df9b` — **after `cov-02`
landed** — one run per phase, default configuration, with
`regression-suite/perf/c2-reach.sh`, which is the "one env var" that answers
*does this workload reach the optimizing tier* before anything about it is
anchored. Counts, so the host's load (1-min 12–50 throughout) does not affect
them.

| phase | requests | admitted | bodies | of the requests, `optimize=false` | tier mgr `c1`/`c2`/`osr` |
|---|---:|---:|---:|---:|---|
| `cb:arithmetic` | 0 | 0 | 0 | 0 | 0 / 1 / 1 |
| `cb:fib` | 1 | 1 | **1** | 0 | 1 / 0 / 0 |
| `cb:sieve` | 2 | 1 | **1** | 1 | 1 / 3 / 2 |
| `cb:matrix` | 1 | 0 | 0 | 0 | 0 / 1 / 1 |
| `cb:hashmap` | 0 | 0 | 0 | 0 | 0 / 1 / 1 |
| `cb:stringregex` | 1 | 0 | 0 | 0 | 0 / 1 / 1 |
| `cb:bintrees` | 3 | 1 | **1** | 2 | 2 / 2 / 1 |
| **CratonBench total** | **8** | **3** | **3** | **3** | |
| `c2c:dispatch` | 15 | 7 | **5** | 6 | 6 / 6 / 1 |
| `c2c:bind` | 9–10 | 5–6 | **2–3** | 4 | 4 / 3 / 1 |
| `c2c:pipeline` | 12 | 5 | **4** | 6 | 6 / 5 / 1 |
| **CratonBenchC2 total** | **36–37** | **17–18** | **11–12** | **16** | |

### `cov-02` moved this table, and the record caught it

The same ten phases on a **pre-`cov-02`** binary (`7c08e9abe`) gave
`cb:sieve` **2/1/0** and a CratonBench total of 8/3/**2**. Closing the array
opcodes gave `sieve` its body: it was dying on `0x54 bastore`, which is now
lowered. So the perf gate's headline reach is **three** bodies across seven
phases, not two — still small enough that the conclusion is unchanged, and
exactly the kind of movement the per-phase record exists to make visible
without anyone re-deriving it.

The candidate moved the other way on the same change, `dispatch` 17/9/6 →
15/7/5, which is a smaller admitted set producing a comparable number of
bodies.

Two more things worth recording rather than smoothing over:

* `stringregex` issues **one** request, where the original survey recorded
  zero — so the CratonBench total is 8, not 7. It admits nothing either way.
* `bind` measured 9/5/2 and 10/6/3 on two consecutive runs of the *same*
  binary. Tier promotion is invocation-count driven against a background
  compiler on a contended host, so a request can land on either side of a
  process's shutdown. Everything else here reproduced exactly. Treat
  single-digit differences as noise and re-run before drawing a conclusion
  from one.

**`compiles_c2` is not this measurement.** The tier manager counts a compile
under the TIER it was requested at, whichever backend produced the body — and
it counts OSR compiles there too. `arithmetic` and `hashmap` each report
`c2=1 osr=1` while issuing **zero** compile requests: that one C2 compile is
the OSR one, entering through `compile_osr_artifact`, which calls the backend
directly and never passes the admission chain. A reader taking `c2=1` as
"the optimizing tier ran here" would be wrong twice over.

### What the candidate is made of

The point of a candidate is not that it reaches the tier; it is that it fails
in the same *places* real code does. The two suites' refusals, same runs:

| refusal | lane | CratonBench | CratonBenchC2 |
|---|---|---:|---:|
| `!scan.typecheck_ops.is_empty()` (`checkcast`/`instanceof`) | `cov-05` | 0 | 5 |
| `!scan.anewarray_ops.is_empty()` | `cov-06` | 0 | 4 |
| `ir.rs:5329` — non-elidable `<init>` on `invokespecial` | `cov-04` | 0 | 4 |
| `ir.rs:5210` — `putfield` whose tag is not `I/Z/B/C/S`, i.e. every reference field store | `cov-03` | 0 | 1 |
| `0x12 ldc` | `cov-01` | 0 | 1 |
| `scan.has_athrow` | `cov-07` | 2 | 0 |
| `!scan.multianewarray_ops.is_empty()` | `cov-06` | 2 | 0 |

The candidate's top three refusals are the survey's top three (306, 138 and 53
events on Spring). CratonBench's are `athrow` and `multianewarray`, which rank
third and last, and it never once touches `checkcast`, `anewarray`,
`invokespecial` or a reference field store. The suites do not merely differ in
how far they get; they disagree about what the optimizing tier's problems are.

Both suites' array-opcode rows are gone from this table since `cov-02`:
pre-`cov-02` the candidate refused once on `0xbe arraylength` and CratonBench
once on `0x54 bastore`. The two `ir.rs` line numbers moved with the same
change (5204 → 5329, 5085 → 5210); they are the same two sites, re-derived,
not new ones.

**This table is revision-stamped and expected to go stale.** It is measured at
`50218df9b`, which has `cov-02` and not `cov-01`; `cov-01` landed immediately
after and takes the `0x12 ldc` row with it. **`cov-04` landed after that and
takes the `ir.rs:5329` row** — the candidate's 4 non-elidable-`<init>` refusals
are now zero, and the site itself no longer exists at that line. Every remaining
`cov-*` lane will move a row here the day it lands — that is the point of them.
Do not patch a cell: re-take the whole column, which is one command per phase
(`regression-suite/perf/c2-reach.sh`), and re-stamp the revision. A table with
one fresh row and six stale ones is the failure this directory's own rules
already name twice.

## Where the 390 die: opcodes `IrBuilder::build` has no arm for

273 events. `[ir] IrBuilder::build has no lowering for opcode 0xNN`.

| opcode | mnemonic | events | lane |
|---|---|---:|---|
| `0xb2` | `getstatic` | 92 | `cov-01` |
| `0x12` | `ldc` | 90 | `cov-01` |
| `0xbe` | `arraylength` | 43 | `cov-02` |
| `0x32` | `aaload` | 18 | `cov-02` |
| `0x13` | `ldc_w` | 7 | `cov-01` |
| `0x5a` | `dup_x1` | 6 | `cov-02` |
| `0x33` | `baload` | 6 | `cov-02` |
| `0x34` | `caload` | 5 | `cov-02` |
| `0x2e` | `iaload` | 3 | `cov-02` |
| `0x54` | `bastore` | 2 | `cov-02` |
| `0xbc` | `newarray` | 1 | `cov-06` |

`getstatic` + `ldc`/`ldc_w` is **189 of 273 — 69%** of every opcode gap.

> **Superseded — `cov-01`, `cov-02` and `cov-04` all closed 2026-08-03.**
> Re-measured on the same three workloads with all three in, this whole table
> is **13 events**, not 273: `newarray` `0xbc` 7, `0x53` 4, `0xb3` 1, `0x5c` 1,
> and every `cov-01` and `cov-02` row is **zero**. The opcode gap is no longer
> where the tier loses methods — `cov-03`'s `putfield` was (78 of the 85
> structural refusals that remained). **`cov-03` closed the same day**, so
> neither is now: on `ConditionalOnPropertyTests` the builder refuses **2**.
> Do not size anything from these rows.

### The asymmetry worth staring at

`IrBuilder::build` **does** have arms for `0x30 faload`, `0x31 daload`,
`0x51 fastore`, `0x52 dastore`. It has arms for **no** integral or reference
array access: not `iaload`, not `baload`, not `caload`, not `aaload`, not
`bastore`.

A float array element can be lowered and an `int[]` element cannot. That is not
a design decision anybody would defend out loud; it is the shape of the
fixtures the arms were written against. It is also precisely the failure mode
this directory's own closeout rule names — *"Do not size a lane from a
fixture's node mix"* — showing up in the code rather than in a plan.

## Where the rest die: structural refusals inside the builder

112 events. `[ir] IrBuilder::build refused at ir.rs:NNNN (bytecode pc N)`.

| site | what it is | events | lane |
|---|---|---:|---|
| `ir.rs:5204` | `invokespecial` that is neither a resolvable call nor a trivial `<init>` to elide | 53 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5085` | `putfield` whose type tag is not `I/Z/B/C/S` — **every reference field store** | 37 | `cov-03` |
| `ir.rs:5264` | an invoke with no `invoke_info` at that pc | 13 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5041` | `getfield` of a `long`/`float`/`double` | 6 | `cov-03` |
| `ir.rs:5314` | — | 2 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5219` | — | 1 | ~~`cov-04`~~ CLOSED |

**The four `cov-04` rows are one cause, not four, and this table says so
misleadingly.** Closed 2026-08-03 —
[`docs/internal/cov-04-the-invoke-arms-RETIRED-20260803.md`](../../internal/cov-04-the-invoke-arms-RETIRED-20260803.md).
Every one of the events is an `<init>`: two whole-method terms discarded the
method's entire `invoke_info` map, and the builder then bailed at whichever
invoke came first in bytecode order. That is why `5264`'s callees are
`StringBuilder.append` and `Class.getName` — sites that were never the problem.
A bail site records where a method died; it does not record why, and a row that
reads like an opcode-shaped gap can be neither.

> **Superseded, and the counts moved in both directions.** With `cov-01` and
> `cov-02` in, this table had grown to **159** events before `cov-04` landed —
> the invoke rows nearly doubling, because a method blocked on `ldc` never
> reached its `invokespecial`. With `cov-04` in as well it reads **85**, and
> only three rows survive: `putfield` **78**, `getfield` **4**, and a `new`
> whose site is `JitNewSite::Deferred` **3**. `cov-03` therefore owns 78 of the
> 85 and is the whole remaining builder story. The methods that were hiding
> behind the invoke terms are constructors, and constructors write reference
> fields. **`cov-03` closed 2026-08-03 as well**, so those 82 rows are gone too
> and the `Deferred`-`new` row is what is left.

**These line numbers are pre-`cov-02`.** Adding the array arms pushed the whole
match statement down; re-derived after it landed, `5204` is now **5329** and
`5085` is now **5210** — the same two sites, and they are the two that `cov-04`
and `cov-03` own. Re-derive the other four before quoting them; a stale line
number in a lane brief sends its first reader to the wrong arm, and this table
is what `cov-03` and `cov-04` are sized from.

`ir.rs:5085` is the second asymmetry. The `getfield` arm was taught to handle
reference fields, and its own comment records why: *"This was the single largest
builder refusal measured on a real workload (66 of 141 on a Hibernate class): a
reference field read is most of what object-oriented Java does."* The `putfield`
arm twenty lines below it was not given the same treatment, and a reference
field **write** is most of what object-oriented Java does too.

> **Both `cov-03` rows closed 2026-08-03.** The numbers above are left as
> measured — this file is a dated record and rewriting it would destroy the
> before-half of every later comparison.
>
> Re-measured three times on `ConditionalOnPropertyTests`, two rounds per arm
> with the order reversed, and the answer grew each time as neighbouring lanes
> landed:
>
> | tree measured on | `putfield` | wide `getfield` | builder refusals | bodies |
> |---|---:|---:|---:|---:|
> | before `cov-01`/`cov-02` | 33 → 0 | 5 → 0 | 83 → 50 | 410 → 445 |
> | + `cov-01`, `cov-02` | 38 → 0 | 5 → 0 | 127 → 89 | 536 → 575 |
> | **+ `cov-04`, i.e. `dev`** | **67 → 0** | **3 → 0** | **72 → 2** | **588 → 660** |
>
> The last row describes `dev` (`fb33aa5ac` → `84b519382`) and its two survivors
> are a `JitNewSite::Deferred` `new` — `cov-06`'s. **No row here is comparable
> to another**, which is why each names its tree: the lane was sized at 43 and
> was 70 when it landed, because `cov-04` admitted the constructors that write
> reference fields. The re-ranking rule this file states corrects **upward** as
> well as downward.
>
> The brief asked which of two candidate reasons the asymmetry actually was.
> It is **the write barrier**, and the compact-layout candidate was not a reason
> at all. Details in
> `docs/internal/cov-03-field-stores-and-wide-fields-RETIRED-20260803.md`.

## The whole-method refusals, before the builder runs

583 events, from `ir_compatible` (`jit/src/ir.rs:6066`). These methods never
enter the builder at all, so they are absent from the 273 and the 112 above.

| conjunct | events | lane |
|---|---:|---|
| `!scan.typecheck_ops.is_empty()` (`checkcast` / `instanceof`) | 306 | `cov-05` |
| `!scan.anewarray_ops.is_empty()` (`0xbd`) | 138 | `cov-06` |
| `scan.has_athrow` | 89 | `cov-07` |
| `!scan.indy_ops.is_empty()` | 44 | — |
| `scan.field_ops.len() > IR_MAX_FIELD_OPS` | 4 | — |
| `!scan.multianewarray_ops.is_empty()` | 2 | `cov-06` |

`indy` has no lane on purpose: `invokedynamic`'s single-pass lowering is an
unconditional trap that publishes a resume snapshot, and giving the IR tier a
second lowering for it is a much larger question than the 44 events justify.
Write it up before starting it, not after.

## Addendum — the `CRATONVM_JIT_FORCE_C2=1` arm

> Measured **before** `cov-01` landed. Its "default" column is therefore the
> pre-`cov-01` tier, and the `0xb2` / `0x12` / `0x13` rows in its opcode table
> are now zero in both arms — see the next section. Its *conclusions* are
> unaffected and one of them was the reason to run `cov-01` first.

Run the same day, same binary, same ten workloads, to answer the question the
`cov-*` lanes rest on: **is a C2 body better than the C1 body it replaces?** If
it is not, widening IR coverage makes things worse faster, and this project has
a recorded case where the optimizing tier was 1.85x slower because fields took
`jit_getfield`.

`CRATONVM_JIT_FORCE_C2=1` treats every compile request as a C2 request, so it
removes the 719 `optimize=false` refusals and routes the whole population
through the optimizing pipeline.

| | default | force-c2 |
|---|---:|---:|
| compile requests | 1,947 | 1,949 |
| `optimize=false` | 719 | **0** |
| refused by `ir_compatible` | 243 | 446 |
| admitted | 984 | **1,502** |
| bodies produced | 595 | **886** |
| **distinct methods with a body** | **495** | **495** |

### Correctness: clean

61/61 Spring Boot tests pass in both arms. All seven CratonBench checksums
match HotSpot in both arms. Zero panics, zero SIGSEGVs, zero internal errors,
zero compile bails, and the same 16 warnings in each — i.e. forcing 886 IR
bodies through this workload introduces nothing.

That is worth stating plainly because it is the first time this tier has been
run at that volume on framework code: the pipeline is *correct* at 886 bodies.

### The finding: FORCE_C2 is not a coverage lever

886 bodies against 595, over **the same 495 distinct methods** — `comm` on the
two sorted method sets reports **zero** on both sides. Not a coincidence of
counts; the sets are identical.

So the extra 291 bodies are repeat compiles of methods the default arm already
lowered. Forcing C2 changes *when* the optimizing tier is used, never *which*
methods it can serve. A method C2 cannot lower, it cannot lower no matter how
early it is asked.

The consequence for this directory is direct: **the `cov-*` lanes are the only
lever on coverage.** There is no configuration that buys what they buy.

### The demand curve is stable, which de-risks the lane sizing

The obvious objection to the main survey is that it measured only the methods
tiering chose to promote — a biased sample. The FORCE_C2 arm is the whole
population, and it says the bias does not matter:

| opcode | mnemonic | default | force-c2 |
|---|---|---:|---:|
| `0xb2` | `getstatic` | 91 | 148 |
| `0x12` | `ldc` | 90 | 132 |
| `0xbe` | `arraylength` | 44 | 68 |
| `0x32` | `aaload` | 18 | 29 |
| `0x13` | `ldc_w` | 7 | 13 |
| `0x33` | `baload` | 6 | 12 |
| `0x5a` | `dup_x1` | 6 | 9 |
| `0x34` | `caload` | 5 | 8 |
| `0x2e` | `iaload` | 3 | 5 |
| `0xbc` | `newarray` | 1 | 2 |
| `0x54` | `bastore` | 1 | 1 |

**No opcode appears in the force-c2 arm that is absent from the default arm.**
The curve scales by roughly 1.5x and the ranking is unchanged;
`getstatic` + `ldc` + `ldc_w` is 293 of 429 events (68%) against 69% before. The
`cov-01`/`cov-02` ordering holds for the full population, not just the promoted
subset.

That prediction was then tested rather than left standing: `cov-01` landed the
same day and took its three rows to zero on the default arm, which moved
`cov-02` to the whole of the remaining opcode gap — exactly the ordering this
table said to expect. Nobody has re-run the force-c2 arm since; when someone
does, the `0xb2` / `0x12` / `0x13` rows should be zero there too, and that is a
cheap check on whether this arm still measures what it says.

The whole-method conjuncts do **not** scale uniformly, and two of them move up:

| conjunct | default | force-c2 | ratio |
|---|---:|---:|---:|
| `typecheck_ops` | 304 | 506 | 1.7x |
| `anewarray_ops` | 134 | 184 | 1.4x |
| `has_athrow` | 85 | **167** | **2.0x** |
| `indy_ops` | 44 | **116** | **2.6x** |
| `IR_MAX_FIELD_OPS` | 4 | 4 | 1.0x |

`athrow` and `invokedynamic` are relatively more common in the methods tiering
does *not* promote. That does not change `cov-07`'s framing — it is still a
question, and "keep the refusal" is still a legitimate answer — but 167 is the
number to argue against, not 85. The same goes for the indy question this
directory currently declines to open at 44: the full-population figure is 116.

### Speed: still not measured, and not for want of trying

The single-run wall clocks favoured force-c2 on all three classes, and a
4x-interleaved re-run of `ConditionalOnPropertyTests` favoured it in 3 of 3
completed pairs. **None of that is evidence.** The host's 1-minute load went
from 9 to 61 during the interleave — the *same* arm measured 67 s at load 24
and 166 s at load 61 — and because the arms ran in a fixed order within each
pair, the decaying spike biased every pair the same way. That is precisely the
confound `feedback_interleave_ab_arms_never_run_them_in_separate_blocks` was
written for, arriving through ordering rather than through blocking.

What can be said: **nothing showed the feared regression.** The 1.85x-slower
shape did not reproduce, at 886 IR bodies, on framework code. That is enough to
say the `cov-*` programme is not self-defeating; it is not enough to say it
pays. Settling it needs `run-cratonbench-gate.sh` on a quiet host — and per
`meas-02`, a workload that actually reaches the tier, which CratonBench does
not.

## Re-measured after `cov-01` landed, same day

`cov-01` closed (`docs/internal/cov-01-constants-and-statics-RETIRED-20260803.md`).
Its three opcodes are at **zero**. Everything below is the same command on the
same workload — `ConditionalOnPropertyTests`, default configuration,
`CRATONVM_DBG=ir-compiles` — with the arms **interleaved, two runs each and in
both orders**. Interleaving matters even for counts: tiering is time-driven, so
the number of compile requests a run issues drifts by a few, and a
block-per-arm layout would attribute that drift to the change.

| | base | after `cov-01` |
|---|---:|---:|
| admitted to the optimizing pipeline | 694–695 | 694–695 |
| **bodies the optimizing backend produced** | **410** | **501–502** |
| `IrBuilder::build returned None` | 281–282 | **180** |

**+91 bodies, +22%**, from 155 opcode-gap events removed. The two numbers do
not match, and the gap is the thing to read: a method that stops failing on
`0x12` immediately fails on whatever it meets next. Most of them met `cov-04`.

All three Spring workloads, same protocol, `cov-01` opcode-gap events
(`0x12` + `0x13` + `0xb2`) in the last column:

| workload | bodies before | bodies after | Δ | gaps before → after | tests |
|---|---:|---:|---:|---|---|
| `ConditionalOnPropertyTests` | 410 | 501–502 | **+91 (+22%)** | 155 → **0** | 38/38 |
| `AutoConfigurationSorterTests` | 123–124 | 142 | **+18 (+15%)** | 23 → **0** | 18/18 |
| `ConditionalOnClassTests` | 57–58 | 63 | **+6 (+10%)** | 12–13 → **0** | 5/5 |
| **total** | **590–592** | **706–707** | **+116 (+20%)** | 190–191 → **0** | — |

The 590–592 figure is the same 592 the headline table reports for all ten
workloads, because the bench phases contribute two bodies and the three Spring
classes contribute the rest. Every arm passes its tests and none crashed.

### `cov-01` **and** `cov-02` together — the number to size the next lane from

The two lanes landed the same day, independently, so neither table above
includes the other's work. Measured on the merged tree, same protocol, against
a `cov-02`-only baseline:

| `ConditionalOnPropertyTests` | `cov-02` only | both lanes |
|---|---:|---:|
| admitted | 696–697 | 695 |
| **bodies** | **442–443** | **537–538** |
| `build returned None` | 247 | **136** |

`cov-01` is worth **+95 bodies on top of `cov-02`** — measured, not inferred
from the two independent deltas. Against the original survey's 410, the two
lanes together are **+128, +31%**.

**The opcode gap is now essentially closed.** Ten events remain on this
workload, down from 196:

| opcode | mnemonic | events | lane |
|---|---|---:|---|
| `0xbc` | `newarray` | 4 | `cov-06` |
| `0x53` | `aastore` | 4 | `cov-02`'s one deliberate exception |
| `0xb3` | `putstatic` | 1 | nobody (the SATB pre-barrier) |
| `0x5c` | `dup2` | 1 | nobody |

Everything else the builder still refuses is **structural**, and two thirds of
it is one lane's:

| site | what it is | events | lane |
|---|---|---:|---|
| `ir.rs:5456` | `invokespecial` that is neither resolvable nor a trivial `<init>` | **72** | `cov-04` |
| `ir.rs:5337` | `putfield` whose type tag is not `I/Z/B/C/S` | 38 | `cov-03` |
| `ir.rs:5516` | an invoke with no `invoke_info` at that pc | 9 | `cov-04` |
| `ir.rs:5293` | `getfield` of a `long`/`float`/`double` | 5 | `cov-03` |
| `ir.rs:5471` | `invokespecial <init>` whose receiver is not a fresh `new` | 1 | `cov-04` |
| `ir.rs:5381` | a `new` with no entry in `new_info` | 1 | nobody |

So the binding constraint the README opens with has **changed**: it is no
longer "opcode coverage in `IrBuilder::build`". It is `cov-04` (82 events),
`cov-03` (43), and the four `ir_compatible` conjuncts, which no opcode lane
touches.

| opcode | mnemonic | before | after | lane |
|---|---|---:|---:|---|
| `0xb2` | `getstatic` | 72 | **0** | ~~`cov-01`~~ |
| `0x12` | `ldc` | 77 | **0** | ~~`cov-01`~~ |
| `0x13` | `ldc_w` | 6 | **0** | ~~`cov-01`~~ |
| `0xbe` | `arraylength` | 28 | 31 | `cov-02` |
| `0x32` | `aaload` | 10 | 12 | `cov-02` |
| `0x2e` | `iaload` | 3 | 6 | `cov-02` |
| `0xbc` | `newarray` | 1 | 3 | `cov-06` |
| `0x5a` | `dup_x1` | 1 | 1 | `cov-02` |
| `0x33` | `baload` | 1 | 1 | `cov-02` |
| `0xb3` | `putstatic` | 0 | **1** | nobody |

**The whole remaining opcode gap is `cov-02`'s** — 51 of 55 events. `putstatic`
appears for the first time: it was always there, hidden behind the `getstatic`
in the same method, and `cov-01` deliberately does not own it (a static
reference WRITE owes an SATB pre-barrier that no collector `set_field` barrier
covers, which is why the single-pass backend keeps writes on their helpers).

The structural refusals are where the shortfall went. Line numbers are
post-change:

| site | what it is | before | after | lane |
|---|---|---:|---:|---|
| `ir.rs:5331` | `invokespecial` that is neither resolvable nor a trivial `<init>` | 36 | **71** | `cov-04` |
| `ir.rs:5212` | `putfield` whose type tag is not `I/Z/B/C/S` | 33 | 38 | `cov-03` |
| `ir.rs:5391` | an invoke with no `invoke_info` at that pc | 8 | 9 | `cov-04` |
| `ir.rs:5168` | `getfield` of a `long`/`float`/`double` | 5 | 5 | `cov-03` |
| `ir.rs:5346` | `invokespecial <init>` whose receiver is not a fresh `new` | 1 | 1 | `cov-04` |
| `ir.rs:5256` | a `new` with no entry in `new_info` | 0 | **1** | nobody |

`cov-04`'s largest refusal **doubled without anyone touching it** — 36 to 71.
That is this directory's own re-run rule playing out one level down from where
it was written: lifting a refusal admits the methods that were hiding behind
it, and they fail on whatever they meet next. `cov-04` is now the single
largest thing between the optimizing tier and the 180 methods it still
declines, and it is the lane the README already says cannot be sized from a
survey.

`ir_compatible`'s whole-method conjuncts moved by 1–2 events each, i.e. not at
all: they are decided before the builder runs, so `cov-01` could not have moved
them and did not.

One knock-on outside the tier, recorded because it is the shape to expect from
every remaining `cov-*` lane: a method moving from C1 to C2 takes it **out of
reach of every single-pass-only capability**. `vm/tests/pgo02_guarded_virtual_inline.rs`
was passing because the IR builder refused `getstatic`, so its
`getstatic; invokevirtual` fixture stayed on the single-pass backend where
guarded monomorphic inlining is planned. It now pins that tier explicitly.

## What this survey does NOT say

* Nothing here is a **timing**. Every number is a count, so the loaded host
  (1-min load 10–14 throughout) does not affect it. No conclusion about speed
  can be drawn from this file.
* It does not say lowering these opcodes makes anything **faster**. It says the
  optimizing tier declines to compile 41% of what it admits. Whether an
  optimized body beats the single-pass one for a given method is a separate
  measurement, and `docs/internal/performance/` has at least one case where it
  did not (`reference_c2_tier_slower_because_fields_take_the_helper`).
* Nine of the ten workloads are Spring-shaped. The bench phases are the only
  counter-sample and they are the row that reaches nothing, so the opcode
  ranking above is a **Spring** ranking. A lane that wants to claim a general
  ranking should add a second family — H2 or Tomcat — before quoting these
  numbers as the ordering.
